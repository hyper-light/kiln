# Resilience: guarded deploys, overload, breakers and recovery

> **Proposed 2026-10-24, for Ada's review.** Written after three outages that day, from a read
> of main at a34c9139. Each gap below has an id (G, O, R, C, F, U, M, P), the incident it caused or
> made worse, a design, and a test that says it's done. §9 orders the work.

## The incidents

- **(a) The poll storm.** The host's wifi dropped. `git ls-remote` failed in about 1 ms, and a
  feedback loop in `gitops::poll_loop` re-polled every repository: 214,803 failed polls in 15
  minutes, about 240 a second. The flood kept the host's resolver at its limit. 473750e5 fixed
  the loop itself.
- **(b) Probes under disk load.** With IO pressure near 80% (concurrent `cargo test` links), the
  server answered probes slowly, kubelet killed it, and the kill wedged the node's dockerd. Kiln
  was down until the node was repaired.
- **(c) The rollback crash loop.**
  1. KilnCD deployed kiln:61b07ceb, which added a column to `cluster_state` outside the
     migrations system.
  2. The guard's memory check saw 2 samples over 80% of the 2 GiB limit (`failures: 1`, counted
     over the whole window) and rolled back to kiln:8adeadba, although serving and steady had
     passed 14 of 14.
  3. 8adeadba couldn't write the wider table and crash-looped.
  4. Nothing in kiln noticed, recorded or reported it. A person rolled forward by hand.

## 1. Shared building blocks

Most of the gaps are the same few mechanisms, missing in many places. They are built once, in
`crates/kiln/src/resil/`, then used everywhere.

- **`Backoff`.** Exponential backoff with full jitter (`rand(0, min(cap, base·2ⁿ))`), a cap,
  and a reset after a stretch of success. It replaces `sup::jitter`, which only adds 0–25% and
  so keeps clients in step.
- **`Breaker`.** One per key: a host (`github.com`), a target (a webhook URL) or a dependency
  (the store, the UI server).
  - **States:** closed, then open (fails at once with "open until T"), then half-open (one trial
    call).
  - **Opens** on a failure rate over a rolling window, with a minimum number of calls, or at
    once on a signal that names a time: 429, `Retry-After`, or GitHub's `x-ratelimit-reset` and
    secondary limits.
  - **Visible:** state, reason and reopen time go to metrics and to `GET /api/resilience`.
- **`Network`.** A breaker over the host's reach itself. When DNS or connect errors come from 3
  or more distinct hosts within 30 s, every outbound call stops. One probe call runs per backoff
  step, and the UI says "kiln can't reach the internet" instead of showing a thousand errors.
- **`Admission`.** A concurrency limit per class of work, with a bounded queue and a queue
  deadline:
  - over the limit, the call is refused at once (HTTP 503 with `Retry-After`; inside the
    server, an `Overloaded` error);
  - limits start fixed (config) and can become adaptive later (AIMD on latency).
- **`Deadline`.** Every request and every background call carries a deadline: store calls,
  outbound calls and subprocesses take it, and the work stops when it passes.
- **`Heartbeat`.** Each long-lived loop ticks a heartbeat. A watchdog (C4) reads them.

**Done when:** `resil` has unit tests for each primitive, including jitter spread, breaker state
changes and admission under contention. Nothing calls `sup::jitter` any more.

## 2. The deploy guard

**G1. Rollback is assumed to work.**

- **Design:** a rollback is a rollout too, and the guard keeps watching it.
  - After it patches the previous image back, the guard watches that version with the same
    `serving` and `steady` checks for a short window (default 3 minutes).
  - If the target fails too, and the new version was serving when it was rolled back (its
    failure was a soft check, G3), the guard rolls forward to the new version and holds.
  - Otherwise it holds and pages.
  - The Job stays alive until the outcome is final, with `backoffLimit` and a deadline sized to
    both windows.
- **Done when:** a test whose rollback target exits at start ends with the new version running,
  the record saying "rollback target failed: rolled forward", and one notification.

**G2. The old version may not run on today's data.**

- **Design:** schema epochs, and every schema change goes through migrations.
  - **Every change is a migration.** All DDL lives in `store/migrations.rs`. A test fails the
    build if `ALTER TABLE` or `CREATE TABLE` appears anywhere else (group.rs:749 moves into a
    migration).
  - **Epochs.** Each build declares `SCHEMA_EPOCH` (what it writes) and `READS_FROM` (the oldest
    epoch it can read). A migration that old builds can't live with (a new NOT NULL column, a
    dropped column, a positional-insert hazard) raises `min_reader_epoch` in a `kiln_meta`
    table.
  - **A refusal, not a crash.** At start, a build whose `SCHEMA_EPOCH` is below the database's
    `min_reader_epoch` refuses to open it, with one clear error ("kiln.db needs a build of epoch
    ≥ 7; this is 6"), served by the rescue listener (C1).
  - **Images carry their epoch.** The image build writes the epochs into a label,
    `dev.kiln.schema-epoch`.
  - **The guard checks first.** Before it rolls back, the guard compares the previous image's
    label with the database's `min_reader_epoch` (read from the new server's `/healthz/server`).
    If the previous image can't read it, the guard doesn't roll back: it holds and pages,
    "rollback blocked: 8adeadba can't read epoch 7".
  - **Expand, then contract.** New columns are nullable or defaulted, and inserts name their
    columns, a rule enforced by a lint over SQL strings. A breaking change ships in two releases:
    expand, then raise `min_reader_epoch` once nothing older runs.
- **Done when:**
  - CI runs a rollback drill: the previous release's binary starts on a database the new build
    migrated. It must either run, or refuse with the epoch error and leave the database
    unchanged.
  - The guard has a test for "rollback blocked".

**G3. Every check has the same power.**

- **Design:** each check has a `severity`.
  - `rollback`: the default for `serving` and `steady`.
  - `hold`: stop the rollout, keep the new version, and ask a person, with Roll back, Keep and
    Extend window in the UI (G8) and in the notification. This would have been the right
    severity for memory today.
  - `warn`: record and notify only.
- **More changes:**
  - Honour `RolloutSpec.auto_rollback`. When it is false, every `rollback` acts as `hold`.
  - Allow `approve:` checks in the guard: it asks through notifications and the UI instead of
    refusing because "nobody is there".
  - A held rollout that nobody answers within `hold-timeout` (default 30 minutes) keeps the
    new version and says so.
- **Done when:** the window tests cover all three severities. A failed `hold` check leaves the
  new image running with the record "held: memory …".

**G4. Two strikes over the whole window is coarse.**

- **Design:** a check states when it fails, in one of three ways:
  - `fail-after: { in-a-row: 3 }`: consecutive samples.
  - `fail-after: { of: 4, in: 2m }`: a rolling window.
  - `fail-after: { for: 1m }`: the condition held continuously for that long.
- **More changes:**
  - `failures: N` keeps its meaning (and stays the default, so no deploy file changes
    behaviour under it); the UI marks it as cumulative.
  - `warmup` (today's `delay`, renamed and defaulted to 60 s for `http` and `metric` checks)
    skips start-up spikes.
  - `aggregate: max | mean | p95` over the samples in a rolling window, when one reading is too
    noisy.
- **Done when:** a single spike, and 2 spikes 5 minutes apart, pass `in-a-row: 3`. Three in a row
  fail it.

**G5. The memory signal is weak.**

- **Design:** `/healthz/server` reports what predicts real trouble.
  - `memory.share` and also `memory.peak` (cgroup v2 `memory.peak`) and `memory.events`
    (`high`, `max`, `oom_kill`).
  - Memory pressure: PSI `memory.pressure` `some avg10` and `full avg10`.
  - The server process's own RSS, separate from its git children.
- **Production's checks become:**
  - **OOM kills:** in `steady`, severity `rollback`.
  - **Memory pressure:** `full avg10 > 10` for 1 minute, severity `hold`.
  - **Share:** over 0.9 for 2 minutes, severity `hold`.
- With the 16 GiB limit (879d7c72), share stops being the binding signal and pressure takes over.
- **Done when:** `kilns_own_checks` covers pressure and peak. The production deploy file uses
  the new checks.

**G6. The reason drops the measured value.**

- **Design:** every sample keeps `value`, and a failure's reason renders it, for example
  `memory: share 0.93 (1.86 GiB of 2 GiB) > 0.8, 3 in a row`. The annotation keeps the last 20
  samples per check, which the UI draws as a sparkline.
- **Done when:** the annotation from a failed rollout shows the values that failed it.

**G7. Nobody is told.**

- **Design:** the guard reports by itself, because it runs in its own Job and can't count on the
  server.
  - It sends kiln's notification events (`deploy.held`, `deploy.rolled_back`,
    `deploy.rollback_failed`, `deploy.blocked`) through a notify sidecar config mounted into
    the Job: the same channels, with the secrets from the kiln Secret.
  - It also writes a Kubernetes Event and the annotation.
  - The deployment record is finished by whichever comes first: the guard (with its outcome),
    or the next server that starts.
- **Done when:** the G1 test's notification arrives with the server down.

**G8. Nothing to see or change without git.**

- **Design:**
  - **Live controls.** The guard re-reads a small override object each tick, the ConfigMap
    `kiln-guard-<deployment>`: `hold`, `release`, `dry-run: [checks]`, `extend: 10m`,
    `roll-back-now`, `promote-now`.
  - **API.** `cd.guard.get` and `cd.guard.act` (audited, admin) write that ConfigMap. They work
    while the server is up. When it is down, `kiln cd guard act` from a terminal writes it
    directly.
  - **UI.** A rollout panel for Kubernetes targets: the phase (watching, held, rolling back,
    verifying the rollback); each check with its severity, policy, last values and sparkline;
    and buttons for the actions above.
  - **Policy editing.** The UI edits a check's policy (severity, `fail-after`, threshold,
    `dry-run`). It saves as a commit to the deploy file, or into kiln for kept apps
    (`cd.apps.keep`), and shows which one.
- **Done when:** a UI test holds a watching rollout, releases it, and dry-runs one check, with
  each action in the audit log.

**G9. No tests for the failures that matter.**

- **Design:** add these guard tests:
  - a rollback target that can't start (G1);
  - the old image on a migrated schema (G2);
  - a hold (G3);
  - a single spike tolerated (G4);
  - the guard reporting with the server down (G7).
- Add the CI rollback drill (G2) to `.kiln/ci.yaml`.

**P1. Live edits are silently undone** (new, found during recovery). Today's `kubectl set
resources` would have been reverted by the next sync.

- **Design:**
  - **Field drift.** KilnCD shows drift per field.
  - **Pins.** A person can pin a field (`cd.apps.pin`, kept in kiln, audited, shown in the UI),
    which the render then layers last.
  - **Ending a pin.** Each pin says "make this permanent": the UI opens the commit that moves it
    into the values file and drops the pin.
- **Done when:** with a memory limit pinned, a sync keeps it and shows the pin in its diff.

## 3. Server overload

**O1. No admission control.**

- **Design:** `Admission` classes in front of the router: `probe` (never limited; see O3), `ui`,
  `api-read`, `api-write`, `hooks`, `git-http`, `agent`, `stream` (long-lived sockets, counted
  separately).
  - **Refusals:** over a class's limit and queue, the server answers 503 with `Retry-After`.
  - **Connections:** a cap on open connections, with accept paused at the cap.
  - **Task slots:** `rt::spawn` returns an error when every shard is full instead of dropping
    the task. Callers handle it: polls are skipped, webhook deliveries stay queued. 10% of slots
    are reserved for `probe` and the supervisor.
- **Done when:** a load test at 10× normal concurrency gets fast 503s with `Retry-After`, while
  `/livez` and `/readyz` stay under 50 ms and nothing logs "task dropped".

**O2. No handler or body timeouts.**

- **Design:** each route class has a deadline: 30 s by default, none for streams and git pushes,
  which use an idle timeout instead. Bodies get a read idle timeout of 30 s. The deadline is
  carried into store calls and outbound calls (`Deadline`).
- **Done when:** a handler stuck on a held store lock returns 503 at its deadline, and its
  connection closes.

**O3. Probes share every bottleneck.**

- **Design:**
  - **`/livez`** is answered by a dedicated OS thread with its own small listener (or the
    supervisor, C1), never by a runtime shard. It reads only heartbeats. It fails only when the
    runtime's heartbeat is over 30 s stale: a real wedge, not a slow disk.
  - **`/readyz`** reads cached state: admission not saturated, the store probe answered within
    2 s in the last 10 s, not draining. A slow store makes it fail readiness (the pod leaves the
    Service) without failing liveness (no kill).
  - **No blocking file reads on shards:** `github_token` is cached and reloaded on change; the
    cgroup reads in `server_health` run on the blocking pool, cached for 1 s.
  - **Probe settings:** the chart and `install_targets.rs` move liveness to `/livez` with
    `timeoutSeconds: 5`, `failureThreshold: 6` and `periodSeconds: 10`, and readiness to
    `/readyz` with `timeoutSeconds: 3`.
- **Done when:** with the store lock held 60 s in a test, `/livez` stays 200, `/readyz` goes 503
  and comes back after.

**O4. Probes don't measure overload or stalls.**

- **Design:** covered by O3's split. Readiness is the overload signal (admission saturated, store
  slow), and liveness is the stall signal (heartbeats). `/healthz/server` adds both, for guards
  and dashboards.

**O5. The store is one lock for everything.**

- **Design:**
  - **Readers.** A pool of read-only WAL connections (4 by default) serves reads in parallel.
  - **Writer.** One writer connection runs on the dedicated `store` blocking share, which
    exists and sits unused today.
  - **Write queue.** Writes go through a bounded queue with a deadline. When it is full, callers
    get `Overloaded` (503 for requests).
  - **No blocking under the lock.** `group.append` never blocks while holding the writer: it
    uses `try_send`, and when the log is full the write is refused with `Overloaded`, never
    parked inside `BEGIN IMMEDIATE`.
  - **Metrics:** store latency by kind, queue depth, lock wait.
- **Done when:** with a slow disk (a fault-injected 200 ms fsync), reads stay fast, writes slow
  down and then refuse cleanly, and `/readyz` reports it.

**O6. Unbounded waits.**

- **Design:** submitting blocking work takes a deadline and fails with `Overloaded` instead of
  retrying forever (`rt.rs:321`). The KilnCD engine's channel gets a bound, and its senders
  handle a full queue. A test lists every unbounded channel, and new ones fail it unless marked
  with a reason.

## 4. Outbound calls: breakers and retries

**R1. No shared breaker or retry, and no view per host.**

- **Design:** every outbound HTTP call and every git subprocess goes through
  `resil::call(key, deadline, f)`, which applies the host's `Breaker`, the `Network` breaker,
  `Backoff` for retries, and `Retry-After`.
- **Retries:**
  - only for idempotent calls, or calls with an idempotency key;
  - 3 attempts at most within the caller's deadline.
- **GitHub's rate limits** open the host breaker until `x-ratelimit-reset`, and a secondary
  limit opens it for its `Retry-After`.
- **Done when:**
  - With DNS gone (a test resolver that fails), all of kiln's outbound calls together stay under
    1 per second after 30 s.
  - The UI and `/api/resilience` show "network down" and the breakers that are open.

**R2. Kicks bypass backoff.**

- **Design:** 473750e5 makes kicks name their repositories and wait out a repository's backoff.
  What's left:
  - discovery stops kicking after a failed run (`gitops.rs:529`);
  - kicks also wait while the host's breaker is open (R1);
  - a webhook storm is coalesced: the set of wanted repositories, not one round per delivery.

**R3. Poll-loop bugs.**

- **Design:**
  - **Stuck in flight.** Clearing the in-flight mark is a drop guard, so no `?` can skip it
    (`gitops.rs:601` in 473750e5 still returns first).
  - **Backoff survives restarts.** Backoff state lives in the store (`repos.fail_count`,
    `repos.next_poll_at`, by migration).
  - **One owner.** The loop's state lives in `Hub`, so a restarted loop doesn't run beside
    orphaned polls.
- **Done when:** a test makes `repo_polled` fail once and sees the repository polled again later.
  A loop restart keeps the backoff and never runs two polls of one repository.

**R4. Long serial stalls in an outage.**

- **Design:**
  - **GitHub checks and statuses:** the host breaker (R1), parallel with a bound of 4, and a
    per-item backoff kept in the store.
  - **Outgoing webhooks:** a breaker per subscriber. After 24 hours open, the subscription is
    disabled with a notice, as GitHub does. Deliveries queued in the store survive restarts.
  - **Slack, Discord and other notifications:** the same breaker per channel.
- **Done when:** with github.com unreachable, the checks loop makes one trial call per backoff
  step, not 200 serial 30 s timeouts.

**R5. Runners reconnect in a herd.**

- **Design:**
  - **Runners:** full-jitter `Backoff` on agent dial and session reconnect.
  - **Server:** a token bucket for agent sessions. Over it, the QUIC handshake is refused with a
    retry-after the agent honours.
  - **Shutdown:** the server's drain (C3) tells connected agents "back in about N s, with jitter",
    so they don't all dial on the first refusal.
- **Done when:** 50 simulated agents reconnecting after a restart spread over at least 20 s, with
  no refusal storm.

## 5. Recovery

**C1. The supervisor never degrades.**

- **Design:**
  - **Bind first.** The server binds its HTTP listener early, in a `starting` state: it answers
    `/livez` and `/startz`, 503 for the rest, and a status page with its progress ("opening
    kiln.db", "joining the cluster group").
  - **Crash-loop state.** The supervisor counts fast exits. After 5 exits under 60 s each, it
    enters `crash-loop`:
    - it keeps restarting with a backoff up to 5 minutes;
    - it serves a rescue listener on the same port: `/livez` 200 (so kubelet doesn't add a kill
      loop on top), `/readyz` 503, and `/status` with the last error, the tail of `crash.log`,
      the schema epochs (G2) and the build;
    - it exposes `kiln_supervisor_state{state="crash-loop"} 1` on `/metrics`.
  - **Fallback.** It extends today's blue/green fallback: when a new binary crash-loops and the
    last good binary can read the database's epoch, the supervisor runs that one and reports it.
- **Done when:** a test server whose child fails at start answers `/status` with the error
  within 5 s, and its metric reads `crash-loop`.

**C2. The crash alerts can't fire in a crash loop.**

- **Design:** alert from outside the process, plus the supervisor:
  - kube-state-metrics (in kube-prometheus-stack): `kube_pod_container_status_restarts_total`,
    `kube_deployment_status_replicas_unavailable`, and waiting reason `CrashLoopBackOff`;
  - a blackbox probe of `/readyz` through the Service;
  - the supervisor's `crash-loop` metric (C1).
- **Done when:** the rules have tests (`promtool test rules`) for each incident's metrics.

**C3. No drain on shutdown.**

- **Design:** on SIGTERM:
  1. readiness turns `draining`;
  2. a `preStop` sleep covers endpoint removal;
  3. new requests are refused;
  4. requests in flight finish within 20 s;
  5. agents are told when to return (R5);
  6. streams are closed with a reconnect hint (WebSocket close 1012);
  7. the store is flushed.
- **Done when:** a rolling restart under load loses no request that wasn't a stream.

**C4. No watchdog.**

- **Design:** a watchdog task, plus a thread the runtime can't block, watching:
  - **runtime lag:** each shard schedules a 100 ms timer and records the overshoot;
  - **heartbeats** of `sup` children: a stale one is logged with its name and restarted after
    a grace period;
  - **the oldest in-flight entries:** polls, syncs and store calls;
  - **blocking-pool queue depth.**

  It feeds `/livez` (O3), metrics, and `/debug/stalls`.
- **Done when:** a child that stops ticking is reported and restarted, and shard lag shows in
  metrics.

**R6. A sender ignores the receiver's budget** (found 2026-10-24 at 20:35): production refused
1,332 frames from one peer, on one lane, in two bursts ("the budget cannot fund the bytes"), and
requests through that runner timed out.

- **Design:** refusal becomes backpressure, not loss.
  - A refused frame tells the sender how much budget is left and when it refills. The sender
    holds that lane's frames until then instead of sending and losing them.
  - Lanes have priorities: control and request lanes are never starved by a bulk lane (lane 6
    here). A busy runner's sessions share its budget fairly.
  - The refusal rate is a metric per peer and lane, with an alert.
- **Done when:** a runner flooding one lane in a test sees its other lanes' requests answered, with
  no refused frames, only slower bulk.

## 6. Front door takeover

Today a front door is one runner that answers DNS for kiln's names and proxies HTTPS to the
server (front-door.md). Several runners can serve the same name, and the tailnet's split DNS lists
each one's nameserver, but:

- each answers DNS with **only its own address**, healthy or not (`frontdoor/zone.rs`);
- a front door that **can't reach the server** keeps answering, and browsers get errors
  (`frontdoor/proxy.rs` has no upstream health);
- on the LAN, DHCP or the router names **one** runner as DNS: when it dies, the whole network
  loses kiln's names, and in resolver mode all DNS;
- moving the front door to another runner is a **manual** re-setup: settings, certificate,
  tailnet DNS.

Taking over the front door is not taking over the server. The server is the one writer
(P1's group of one voter); approved `p2p.md` P3 (voters on runners) and P4 (leases) make it
replaceable. This section ships first and works without them:

- before P3, a takeover front door serves **reads** from its learner replica (P2), and writes are
  refused with a clear "read-only: kiln's server is unreachable";
- after P3, it serves everything.

**F1. Every front door answers with the healthy ones.**

- **Design:** the front doors serving a name know each other, through the server's push today
  and the cluster group after P4.
  - **Answers.** Each front door's DNS answers with **every healthy front door's address** on
    the asker's network (LAN askers get LAN addresses, tailnet askers get tailnet addresses), its
    own first when it's healthy. The TTL drops from 60 s to 20 s.
  - **Failing over.** Browsers try the next address when one refuses or times out, so a dead
    front door costs one failed connect, not an outage.
  - **Health** is measured by the front door and reported by its peers. All of these count:
    - its listeners are bound and its certificate is valid;
    - it can reach the server (`/readyz` within 2 s, in the last 10 s), or it holds a learner
      replica less than 30 s behind;
    - its peers reach its 443 (each front door probes the others every 10 s).
  - **Withdrawing.** An unhealthy front door drops itself from every answer, its own included,
    and keeps answering with the healthy ones. A front door its peers can't reach is dropped
    from their answers within 30 s.
- **Done when:** with two front doors and one killed, `dig kiln.dev` on the LAN and on the
  tailnet returns only the live one within 30 s, and a browser test reloads without error.

**F2. The LAN's single DNS server.**

- **Design:** no single address for the network's DNS.
  - **Two DNS servers.** The setup page tells the person to give DHCP two front doors as DNS
    servers (most routers allow two or more). For authoritative mode with conditional
    forwarding, the router forwards the domain to both.
  - **An optional floating address** (Linux front doors, opt-in, needs `CAP_NET_ADMIN`). One
    front door holds a LAN address, say 192.168.1.53, through a lease with a fencing number:
    - it announces the address with gratuitous ARP;
    - when its lease lapses (no renewal for 10 s), the next front door takes the address;
    - the router then needs only that one address.
  - **Resolver mode safety.** A front door in resolver mode that can't reach its upstream
    resolvers answers SERVFAIL at once, so clients fall back to the second DNS server instead of
    timing out.
- **Done when:** with DHCP handing out two front doors, killing one leaves name resolution on a
  test client working with no change on the client.

**F3. A front door that can't reach the server.**

- **Design:**
  - **Through a peer.** A front door that can't reach the server, but whose peer front door can,
    forwards to that peer over the runners' links (reach), before giving up.
  - **Read-only.** With no path to the server, a front door holding a replica serves reads from
    it. Writes get 503 with `Retry-After` and a JSON reason. The UI shows a read-only banner
    (U1's breaker).
  - **No replica.** It withdraws (F1) and serves a static "kiln is unreachable" page to
    connections already open.
- **Done when:** with the server stopped, a front door with a replica serves the runs page and
  refuses a write with the read-only reason. With the server reachable only through a peer, pages
  load.

**F4. Moving the front door is manual.**

- **Design:** front-door settings belong to the **name**, not to one runner: a domain has
  members (runners) with a role each:
  - **`active`:** serves the name and answers DNS;
  - **`standby`:** listeners ready, certificate issued, excluded from answers until promoted;
  - **`off`.**
- **Taking over.**
  - **Automatic.** When no `active` member is healthy for 30 s, the healthiest `standby` becomes
    `active`. One member is allowed to do so at a time, through a lease (the server's grant
    today, the cluster group's after P4), and the lease carries a fencing number.
  - **By hand.** "Make this the front door" in the UI promotes a member:
    1. the new one goes `active`;
    2. it is checked from the tailnet (`100.100.100.100`) and the LAN;
    3. only then does the old one go `standby`. There's no gap.
- **Certificates.**
  - **Self-signed:** each member gets its own from kiln's root, as today, so a takeover needs
    nothing.
  - **Uploaded:** kept for the name and sent to every member, so a standby can serve it.
- **Tailnet DNS** lists every `active` member's nameserver.
  - The server's tailnet sync drops a member whose health fails for 2 minutes, as well as
    removing turned-off ones.
  - So the route survives the server being down, members hold the Tailscale key sealed (after
    P5) or a scoped token for the DNS call only (before P5): see the questions below.
- **Done when:** the runner holding `active` is stopped, a `standby` takes over within 60 s, the
  tailnet and the LAN resolve to it, and the UI keeps working after one reconnect.

**F5. The UI doesn't know where it is.**

- **Design:**
  - **Which front door.** Every response carries `kiln-front-door: <runner>` and
    `kiln-mode: full | read-only`. The UI shows the front door it's on and the mode.
  - **On loss.** When its front door dies, the UI's breaker (U1) reconnects, and the browser's
    next connection resolves to a live member (F1).
  - **Shared links keep working.** Links and sign-in cookies are for the name, not the runner,
    so they work through any member (sessions are already in the cluster group).
- **Done when:** a Playwright test against two front doors kills one and sees the banner, the
  reconnect, and the page working through the other.

**F6. Seeing and choosing front doors.**

- **Design:** Runners → Domain & HTTPS becomes per name:
  - its members, roles and health (the checks in F1, from each member and its peers);
  - the lease holder, and the last takeover with its reason;
  - the actions: promote, stand by, take out;
  - every change in the audit log (`frontdoor.*`), and a notification on every automatic
    takeover.
- **Done when:** a takeover shows in the UI, the audit log and the notifications.

**F7. Today's front door has no second.** adacachy alone serves `kiln.dev`; the Mac and fedora
refuse 443 over the tailnet.

- **Design (operations, not code):**
  - Turn on a second front door, as `standby` once F4 exists (until then, a second `active`):
    fedora is steady and never sleeps.
  - Add both to DHCP's DNS servers (F2), and trust kiln's root on the devices once, as for
    adacachy.
- **Done when:** `dig @fedora kiln.dev` answers and `https://kiln.dev` loads through fedora with
  adacachy's front door stopped.

## 7. UI

**U1. No fetch timeout or retry; 503, 429 and Retry-After are ignored.**

- **Design:**
  - **Timeouts.** `request()` takes a timeout: `AbortSignal.timeout`, 15 s by default, set per
    call.
  - **Retries.** Idempotent reads retry on network errors, 502, 503 and 504 with full-jitter
    backoff, honouring `Retry-After`. Writes retry only when they carry an idempotency key,
    which the server would accept on every write.
  - **A shared breaker for the server.** Kept in one place for every tab, through a
    `BroadcastChannel`:
    - after 3 consecutive failures, or one 503 with `Retry-After`, it opens;
    - polling pauses, and a banner says "kiln isn't answering; retrying in N s";
    - one probe of `/readyz` runs per backoff step;
    - half-open, it resumes.
- **Done when:** a Playwright test with the mock server returning 503 sees one probe per step
  across 3 tabs, the banner, and recovery.

**U2. Polling continues at full rate while the server is down.**

- **Design:** resources use the shared breaker. A key that errors backs off itself (doubling up
  to 5 minutes), and polling pauses while the breaker is open.

**U3. Reconnect causes a refetch herd.**

- **Design:**
  - **Spread the refetch.** On reconnect the UI refetches visible keys first, spread over 0–5 s,
    and the rest later.
  - **Event socket.** It backs off up to 60 s with full jitter. Online and visibility changes
    trigger one probe, not an immediate reconnect of everything. The server's close code 1013
    ("try again later") with a delay is honoured.
- **Done when:** after a server restart, the UI's requests over the first 10 s stay under twice
  their steady rate.

**U4. Stale data isn't shown as stale.**

- **Design:**
  - **Stale marks.** Every resource shows its age once it is older than twice its poll interval,
    with the last error on hover (`refreshError` rendered).
  - **Reconnecting sockets.** The log viewer and the deployment sockets reconnect with a resume
    cursor (the last sequence seen), so lines aren't lost or doubled.

**U5. No timeouts on the Bun UI server or its forwarder.**

- **Design:**
  - **Bun UI server.** `idleTimeout` set, and an `AbortSignal.timeout` on every upstream API
    call, 10 s by default.
  - **Rust forwarder.** A deadline (O2) and a `Breaker` on the UI server. When it is open,
    pages fall back to the shell at once, as they do today when it is down.

## 8. Observability

**M1. Missing metrics.**

- **Design:** add metrics for:
  - HTTP requests, latency and in-flight, by class (O1);
  - admission refusals;
  - store latency, queue depth and lock wait (O5);
  - blocking-pool depth, and runtime lag per shard (C4);
  - outbound calls by host and result, and breaker state (R1);
  - git subprocesses started, by command;
  - supervisor state (C1);
  - guard phase and check values (G6).

**M2. The poll alert can't see a storm.**

- **Design:**
  - **Storm alert.** A rate alert, `rate(kiln_gitops_polls_total[1m]) > 3 × repositories /
    poll interval` for 2 minutes, which would have fired for (a) within 3 minutes.
  - **New alerts:** a breaker open over 10 minutes; `Network` down; admission refusals over 1%;
    store p99 over 1 s; runtime lag over 1 s; supervisor `crash-loop`; a held or blocked guard.
- **Done when:** `promtool` tests feed each incident's series and see the alert.

## 9. Order of work

Each phase ships on its own, through the usual gate and train. Earlier phases close the gaps
that caused today's outages.

| Phase | What | Gaps | Why first |
|---|---|---|---|
| 0, now | Production's deploy file: memory check `severity: hold` once G3 exists (until then `dry-run: true`); external alerts; probe timeouts in the chart; R3's drop guard; discovery stops kicking after a failed run; a second front door on fedora and two DNS servers in DHCP | C2, O3 (settings only), R2, R3, F7, F2 (operations) | Small, safe, stops a repeat of (a), (b) and (c), and of a dead front door |
| 1 | Guard safety: watch the rollback, epochs, severities, `fail-after`, values in the reason, self-reporting, tests | G1–G7, G9 | Today's outage: a soft check became a crash loop |
| 2 | `resil` primitives; outbound calls through them; network breaker; lane backpressure | R1, R4, R5, R6, M1 (outbound) | The storm class of incident, everywhere, not only polls |
| 3 | Front doors answer with the healthy ones, reach the server through a peer, serve read-only from a replica, and say where the UI is | F1, F3, F5 | A dead front door or a lost link costs one reconnect, not an outage |
| 4 | Probe split, admission, deadlines, store readers and writer, watchdog, drain, crash-loop state | O1–O6, C1, C3, C4, M1 | Slow disk mustn't look like death |
| 5 | UI: fetch, shared breaker, backoff, stale marks, reconnects, UI server timeouts | U1–U5 | The UI stops adding load when kiln is down |
| 6 | Front door roles and takeover (lease, standby, promote), the floating address; guard controls and policy in the UI; KilnCD pins | F4, F6, F2 (floating address), G8, P1 | Configure and move things without a commit during an incident |
| then | `p2p.md` P3 (voters on runners) and P4 (leases): a takeover front door serves writes too | — | Taking over the server, not only the front door |

Open questions for Ada:

1. **Severity for memory in production:** `hold` (ask a person), or `warn` now that the limit is
   16 GiB?
2. **Hold timeout:** keep the new version after 30 minutes unanswered, or roll back?
3. **The rescue listener (C1):** in the supervisor (one more job for it), or in a tiny separate
   process?
4. **Pins (P1):** allowed for any field, or only a list of safe ones (resources, replicas,
   env)?
5. **Front doors and Tailscale's key (F4):** before P5 seals secrets in the cluster group, may
   front doors hold a token scoped to the tailnet's DNS call, so the routes follow a takeover
   with the server down? Or do routes wait for the server?
6. **The floating LAN address (F2):** worth the privilege (`CAP_NET_ADMIN`, Linux only), or are
   two DNS servers in DHCP enough?
7. **Automatic or by hand (F4):** should a standby take over by itself after 30 s, or only on a
   person's word, with a notification asking for it?
