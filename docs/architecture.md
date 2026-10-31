# kiln — architecture

kiln is a self-hosted, GitOps CI/CD job server for the hyper-light stack. One static binary carries
the server, the agent, the process supervisor, the installer, the CLI and the embedded web UI. It
runs the same way on bare metal and on Kubernetes.

## Goals

| Requirement | How kiln meets it |
| --- | --- |
| GitOps, pulls from GitHub | Server polls registered repositories (conditional requests, ETag; 304s are free against the rate limit) and optionally accepts webhooks. Each is polled on its interval, and at once when a push or a webhook names it; a failed poll backs off (from 10 s, doubling, to 5 minutes), so an outage can't make the poller spin. Pipelines live in the repo at `.kiln/*.yaml`. |
| Rust / Python / Go / TypeScript (+ Erlang) | Toolchain presets pick images and caches; steps are plain shell so anything installed works. |
| Heavy concurrent load | Async scheduler on hyper-rt (thread-per-core shards), HTTP/1.1 and HTTP/2 from kiln-http ([http.md](http.md)), bounded per-agent slots, per-repo concurrency groups, back-pressure. Logs go to disk segment files, never through the database. |
| In-cluster **and** on-hardware jobs | One executor: the `kiln agent`. On bare metal it is a long-lived daemon; on Kubernetes the server launches an ephemeral agent pod per job. Jobs choose with `runs-on` labels (`k8s`, `baremetal`, `bench`, `gpu`, …). |
| hyper-raft QUIC + TLS | Agent⇄server and CLI⇄server traffic runs over hyper-raft's QUIC transport with mutual TLS from hyper-tls. Amphora owns record replication through hyper-raft; transport connectivity does not confer record authority. The browser uses HTTPS + WebSocket (browsers cannot open raw QUIC). |
| Rich, cheap telemetry | `/metrics` Prometheus endpoint; host/job samples read straight from `/proc` and cgroup files at a configurable low rate; agents ship samples as QUIC datagrams. No sidecars, no agents-of-agents. |
| Grafana / Prometheus / ArgoCD | `deploy/` holds a Helm chart for kiln, a ServiceMonitor, Grafana dashboards as ConfigMaps, and ArgoCD Applications for the whole stack. |
| Full machine control from the UI | PTY-backed terminal sessions (xterm.js + WebGL), full TTY semantics: resize, raw mode, alt-screen TUIs (k9s, htop, vim). |
| Starts on boot, self-healing without systemd/K8s | `kiln supervise` is a tiny parent process that restarts the server with exponential backoff and reaps zombies (works as PID 1). Boot hook: systemd unit if present, else `@reboot` crontab, else XDG autostart. Inside the server, every subsystem is a supervised task that is restarted on error (Erlang-style). |
| Never panic | Workspace lints deny `unwrap`, `expect`, `panic!`, `todo!`, `unimplemented!`, unchecked indexing. Task boundaries additionally use `catch_unwind` as a last line of defence. |
| Containerizable, single distribution | One file: a static musl binary on Linux. It embeds the UI's static files (TanStack Start's client build, with `rust-embed`) and carries the UI server: Bun, pinned in `vendor/bun`, and TanStack Start's server build, which render pages and React Server Components. The server unpacks and runs them itself (see [The web UI](#the-web-ui)). |
| Install with or without passwords | `kiln install` is interactive by default; `--unattended` never prompts (uses `sudo -n` or user-level install only, generates secrets itself and writes them `0600`). |
| No paid services | Everything here is open source and self-hosted. |

## Components

```
                   ┌──────────────── kiln server ────────────────┐
 GitHub ◀─poll/ETag─┤ gitops   CI dispatch   Amphora Database     │
        ──webhook──▶│ api(HTTP+WS)  terminal(PTY)  metrics  logs  │
                   └───────▲──────────────▲──────────────▲───────┘
                     HTTPS/WS        QUIC + mTLS        QUIC + mTLS
                         │               │                  │
                    browser UI     kiln agent (bare     kiln agent pod
                    (embedded)     metal, bench box)    (per job, k8s)
```

### The web UI

The UI (`ui/`) is a TanStack Start app with React Server Components.
- **Static files:** the kiln server serves the client build (`ui/dist/client`, embedded) itself, and
  keeps earlier builds' assets so pages loaded before a deploy keep working (`crate::ui`).
- **Pages, server functions and server components:** these come from the UI server, TanStack
  Start's server build (`ui/dist/server`) run by Bun (`ui/server/main.ts`). The kiln server starts it
  on a loopback port, forwards the UI's requests to it with streaming both ways, and starts it
  again if it exits (`crate::uiserver`). The UI server stops when kiln does.
- **kiln carries it** (`vendor/bun/README.md`): `npm run build` packs Bun, pinned per platform in
  `vendor/bun/bun.json` and checked against it, with the UI server's files (`ui/dist/embed/`), and
  the kiln binary embeds them. A server unpacks them once into `<data dir>/ui-server/<hash>/` (a
  folder named for what's in it, renamed into place once whole) and runs them. On Linux, Bun's musl
  build runs through the musl loader it came with, so it needs nothing installed, on glibc or musl.
- **Without Bun or the UI server's files:** every page is the app's shell (`_shell.html`, rendered
  at build time), and the app runs in the browser. That's a kiln built without them
  (`KILN_UI_SERVER=off`, or a build that couldn't download Bun, which says so).
- **The API stays with the kiln server.** Server functions reach it on its own listener, carrying
  the visitor's cookie.
- **Where it looks:** a Bun and files of your own first, when `KILN_BUN` (default: `bun` on the
  PATH) or `KILN_UI_DIR` (default `/usr/local/lib/kiln/ui`) is set. Then the one kiln carries,
  unpacked into the data directory, or into `KILN_UI_SERVER_DIR` (a private folder that lets
  programs run, for a data directory mounted noexec: kiln says so when it can't run from there).
  Then `/usr/local/lib/kiln/ui`, where older images put them.
- **HTTP/1.1 and HTTP/2:** the kiln server's listener speaks both (`crate::http::server`). Over TLS
  (`server.https`) the browser picks by ALPN. In the clear, HTTP/2 opens with its preface, which is
  how the front door passes a browser's HTTP/2 on. Handlers see every request shaped as HTTP/1.1's:
  `Host` from `:authority`, one `Cookie`, the target as a path. WebSockets stay on HTTP/1.1.

Every page is a route of its own (`ui/src/routes/_app.*.tsx`), rendered in the browser for now.
Pages become server components where that keeps JavaScript out of the browser; the shared plan page
(`routes/shared.plans.$token.tsx`) is the first.

### Pipeline definition (`.kiln/*.yaml`)

```yaml
name: ci
on:
  push: { branches: [main, "release/*"] }
  pull_request: {}
jobs:
  test:
    runs-on: [k8s]
    toolchain: rust            # preset image + cargo cache
    matrix: { profile: [dev, release] }
    steps:
      - run: cargo test --workspace --profile ${{ matrix.profile }}
  bench:
    needs: [test]
    runs-on: [baremetal, bench]
    steps:
      - run: cargo bench -- --save-baseline ${{ git.sha }}
    artifacts: [target/criterion/**]
```

### Ownership (U19)

This maps current source ownership, not final acceptance or production activation. Final combined
gates remain the acceptance boundary; individual checks do not establish whole-goal completion.
Plans and goals live in kiln.

| Concern | Canonical owner and source |
| --- | --- |
| Workflow parsing, planning, scheduling and routing | Pure `crates/kiln-ci` (`wf`, `plan`, `sched`); `crates/kiln/src/ci` supplies events, persists decisions and dispatches assignments. |
| Shared CI/reach admission | `ci/dispatch.rs`, `ci/dispatch/admission.rs` and `kiln-ci::sched` own one capacity scheduler; reach reservations enter that scheduler rather than making independent placement decisions. |
| Cron, ref/webhook ingress and outgoing fanout | `ci/schedule.rs`, `ci/events.rs`, `gitops.rs` and `ci/hooks.rs` accept immutable sources through the existing SQL outbox (`ci/schedule/outbox.rs`); `ci/webhooks.rs` freezes outgoing recipient fanout in the same materialization transaction. Quiver only wakes consumers. |
| Cancellation acceptance | `ci/dispatch/cancellation.rs` acknowledges the captured attempt only after its existing `ci_plan_log` intent commits; retries/recovery retain that receipt. Physical stop proof remains the execution owner's responsibility. |
| Step execution | `crates/kiln/src/runner/runtime.rs` executes workflow assignments using `kiln-ci` decisions; `runner/workspaces.rs` holds stable cross-process locks through cleanup and refuses failed claims. Legacy inputs adapt to this runtime rather than owning another scheduler or execution engine. |
| SQL and Records | One host Amphora `Database`, injected from `Store.database()` across Hub, Host and agent (`store.rs`, `records.rs`), with SQL migrations and isolated Records namespaces; namespaces do not create independent databases or writers. `store/migrations.rs` initializes schema through checked durable per-migration steps, using one fenced inventory read on warm startup. After `UnknownOutcome`, only an exact receipt from a fresh fenced read proves commit; absent or failed reconciliation leaves startup unready. |
| Structured sessions and conversations | `harness/registry.rs` injects per-host server/machine Records namespaces, global `registry` control and `visuals`; `convo::from_host_sources` binds legacy origins to that same database for conversation data and feeds. Persistent local `harness/host.rs` start/stop ledgers own acceptance; remote close succeeds only on durable runner `Taken`, never in-memory `Sent`/`Parked` (`harness/registry.rs`). |
| CD control | The pure `kiln-cd` engine decides; `cd` performs effects and persists control in the shared database (`cd/store.rs`, canonical Records in the `cd-engine` namespace). |
| Secrets | `secrets/{mod,kiln,crypto}.rs` own versioned storage and encryption; `ci/secrets.rs` applies CI scope, trust and broker policy against that shared service. `secrets/crypto/bootstrap.rs` validates/caches the master before other Hub effects and `Store::recover`, detects the actual replicated owner, requires an existing key plus trusted pin where appropriate, and uses an immutable verifier to refuse rebinding. This correction is frozen/source-ready; migration is described in [ci.md](ci.md). |
| Environment gates | `kiln-ci::env` decides protection policy; `ci/environments/{source,policy,actions}.rs` adapts definitions, approvals and transitions, with CD consuming the same environment boundary. `ci/runs.rs` validates exact deployment/run/leg/attempt/environment/repository/SHA context before committing deployment control; `ci/notify.rs` publishes afterward. |
| Kubernetes access | `kubernetes.rs` owns configuration, credentials and connections; `kubeapi.rs` and `cd/kube.rs` adapt typed executor and CD operations through kiln HTTP. |
| Operations and agent UI | `ops.rs` registers shared authorized operations; `harness/ops.rs`, `api.rs` and `ui/src/views/harness` expose structured ownership without becoming separate control stores. `ui/src/lib/data.ts` owns `harnessSessionsRes`; waiting cards project that same roster through `observeResource` (`ui/src/views/harness/waiting.ts`), without another fetch/event owner or markup change. |
| Runner CI/CD upgrades | `updates.rs` and `updates/coordinator.rs` persist lifecycle state and consume immutable `build_commit` artifacts with signed provenance; `cd/runners.rs` submits release-commit work through CI. `upgrade.rs` supplies physical replacement mechanics. |
| Host control metadata | Sync places/tasks and reach copy inventory/requests use host-scoped SQL control cells (`sync/places/control.rs`, `reach/copies.rs`); keeper state (`reach/keeper.rs`), runner clone provenance (`sync/runner.rs`), agent-limit policy (`harness/limit.rs`) and front-door policy/import receipts (`frontdoor/settings.rs`) use the existing machine Records namespace from the host database. Front-door `settings.json` is read-only legacy input; certificate/key files remain physical. |
| Kept files and terminal names | `harness/kept.rs` owns file-list metadata and `termsess.rs` owns terminal names in Records injected from the same Amphora database; byte files and terminal processes remain physical resources. |
| Notifications | `bus.rs` uses Quiver for event delivery; notifications are separate from canonical records and their replication. |
| Record replication | Amphora's hyper-raft integration is the sole record replication authority. `replication/carrier.rs` polls bounded queued-receive receipts while driving egress: transport admission is not a processed or committed acknowledgment. Reach links and front-door routing do not add another record log. |
| Cache/object metadata and bytes | `ci/services/{cache_v1,cas,chunks}.rs` and `blobs` own cache/object access; durable metadata belongs to the shared Amphora store, while content bytes remain in object storage and runner caches (`runner/cache.rs`). The runner shares one optional bounded chunk tier (`runner/cache.rs`, `runner/work/store.rs`), with canonical Amphora accounting and temporary OS-owned operation guards; logical holdings never promise byte residency. Its physical mutation fence covers publication versus trim; `ci/registry/{mod,proxy,mirror}.rs` retain staging until the last writer/owner releases it. These resource fixes are frozen, not final-gate acceptance. |
| Git mirror/fetch | `git.rs` adapts credentials, timeouts and cancellation to `kiln-cd/src/git.rs`'s shared read-mirror engine; physical mirrors are keyed by source and effective credential identity, not independent control records. `cd/mod.rs` bounds credential prefixes by authority and repository path; that fix is ready. Shared read-mirror mutations use a stable sibling `.kiln-read.lock` across processes alongside the in-process gate; lock waits use the bounded `git-mirror-lock` blocking-pool share. Source implementation is present; final integration/native validation remains pending. |
| Files, logs and blobs | Synctree, sync logs, object stores and reach carry bytes and checkpoints. Their transport positions describe byte availability, not accepted control intent. `harness/kept.rs`'s cross-process `.publication.lock` fences physical blob publication/GC through metadata commit; the lock is not a metadata authority. |
| Session persistence versus physical controls | `harness/registry.rs`, `harness/host.rs` and `harness/session.rs` persist structured intent through injected stores; `harness/survivors.rs`, `os/contain_*` and `runner/kept.rs` manage process identity, kernel/container fences, journals and locks as local proof/bootstrap, not canonical accepted intent. |

Legacy SQLite/Turso and JSON sidecars are read-only import inputs. Imports are checked and
restartable; ambiguous or partial ownership fails closed, without falling back to a legacy writer.
Local bootstrap material and physical locks remain where the operating system needs them.

Runner upgrades (`updates.rs`, U16) consume immutable CI artifacts from `build_commit` with signed
provenance; `cd/runners.rs` routes CD work through CI at its release commit. Ad hoc scripts are not
an alternative release authority. `upgrade.rs` supplies replacement mechanics; the person still
controls runner replacement and timing. This mapping does not report an upgrade performed.

Coding-agent discovery (`aisessions.rs`, U18) uses the shared Tool registry across harnesses.
External interactive processes are observations without control ownership. Structured sessions and
their descendants belong to the structured host: a verified `KILN_AGENT` session mark and bounded
process ancestry establish that ownership, never a bare reusable PID.

### Runner cache policy and cutover (U07)

The runner stores each cached digest once under `<work>/_cache`; derived assembly manifests
remain under `_cas/objects`. Scoped logical ledger metadata stays in the existing canonical
Amphora database and Records/ledger boundary. Physical accounting uses three relational record
sets in that same database: `runner_cache_tiers` (root identity, budget and cursors),
`runner_cache_chunks` (chunks and `manifest:<digest>` inventory), and `runner_cache_intents`
(publication reservations). These are not another Records backend or a business reference graph.
`_cas/index.log` and `_cas/ledger.db` are read-only migration inputs, not live metadata writers.

All idle chunks and manifests share an LRU, including bytes named by current holdings, replaced
or deleted entries, failed publications and historical results. Canonical history survives byte
eviction; authorization does not imply residency. Missing or corrupt optional bytes require
refetch/recomputation. Verified corrupt copies are discarded rather than retained in an uncharged
quarantine. `[agent] cache_max` (default 10 GiB) charges unique chunks, derived manifests and owned
publication/repair staging. Reserve the complete footprint before writing; repair charges the
old bytes plus replacement peak. Lowered limits or imported overage can need multiple bounded
maintenance passes; growth is refused until used plus reserved plus requested bytes fit.

Only active operations protect bytes. The engineering limits are 64 OS-owned lock/header slots
per physical root, at most 65,536 digests per operation, and a fixed header bound of the format
prefix plus 65 bytes per digest. Clones and covered nested reads reuse ownership through dispatched
IO and cancellation; process death releases it. Stale headers and old permanent pins establish no
liveness; malformed active headers refuse eviction. Assembly manifests are limited to 8 MiB.
Typed `Capacity`, `MaintenancePending` and `GuardCapacity` permit optional-cache bypass where
supported; mutation Busy uses bounded caller retry/bypass. IO, metadata/commit, format and invalid
publication errors remain failures. An optional cache decline never acknowledges unpublished work.

Workfill/transport input staging is separately bounded to 16 GiB per transfer, not charged-tier
residency and not a global concurrent-staging budget. Plan free disk for concurrent transfers,
cache publication peaks and database/workspace use together. Peer reads use guarded shared chunks
without permanent whole-object peer copies. Neither this limit nor `cache_max` bounds the entire
work directory.

The following is a reviewable activation procedure, not authorization or automatic activation.
U07 source is frozen and source-reviewed; compile, profile/performance and native validation remain
pending. No runtime or gate PASS is claimed here.

1. Before approval, attach exact-source validation and a recovery/rollback drill to the release
   review. Inventory each work root, its canonical database identity, budget, legacy inputs and
   live agents/terminals/jobs. Preserve consistent canonical metadata and any pinned source needed
   by an incomplete metadata import; external recovery copies are not active cache residency.
2. Leave runner replacement and timing to the person under the existing release procedure.
   Before pre-format writer cutover, an operator must verify that **all** old writers sharing the
   root are quiescent and cannot resume during migration. Age, absent new locks or old pin files
   are not proof. Only then may the authorized operator create `<work>/_cache/.u07-quiescent`
   containing these exact UTF-8 bytes, including the final LF:

   ```text
   U07: all pre-format cache writers are quiescent
   ```

3. Initialization performs a restartable, idempotent, paged one-time inventory of chunks and
   manifests before new admission. The physical fence protects inventory/publication; the
   `.u07-format` identity binds the root to the canonical database. Do not erase a mismatched
   format binding or import receipt to force readiness. A fresh empty tier needs no attestation;
   legacy bytes, manifests, staging or pins require the offline proof.
4. Quiescent cleanup removes only recognized legacy digest-named pins, corrupt/peer copies,
   32-hex `_cas/tmp` files and known digest/publication-ID temporary files. Unknown entries refuse
   readiness and need explicit identification, never speculative deletion. Old `_cas/chunks` and
   whole-object sources are verified and copied into the charged tier or discarded on typed
   admission decline; corrupt optional sources are discarded. Whole legacy sources above their
   8 MiB format contract refuse import. No permanent uncharged legacy cache floor is retained.
5. On interruption, reopen with the same database and supported import/recovery path. Preserve
   pinned metadata sources until their receipts finish; do not resume legacy writers as fallback.
   Canonical intents retain reservation credit until fenced recovery reconciles files. Before
   admitting work, review readiness, used/reserved bytes, deferred maintenance and free space.
   Wrong binding, unknown files, IO or metadata errors require diagnosis. A binary downgrade is
   not a proven rollback: old writers do not understand the new fences or accounting.

### SQL format cutover and rollback

A SQLite-era `kiln.db` cannot activate automatically beside the canonical Amphora owner.
`kiln store cutover --data-dir <explicit-directory> --offline` is the supervised offline entry:
all old SQL/group/backup producers and their automatic restart must actually remain disabled
through interruption. The assertion and file lock do not establish that authority. It preserves
the read-only source inside a directory at `kiln.db`, which blocks both old SQLite opens and old
replica rename-over replacement. New acceptance requires that fence and the exact completed
Amphora restore identity. Windows cutover is currently typed unsupported before mutation because
this implementation cannot prove durable directory publication there. Follow the
[SQL cutover train](trains/unification-sql-cutover.md); a SQLite-era binary is a roll-forward-only
boundary, not a safe image rollback after canonical writes.

### Run lifecycle

CI records workflow runs and attempts; `kiln-ci::sched` decides assignments, waits, concurrency,
resource routing and retry policy from supplied events. Server dispatch carries out those commands,
and runner events advance the durable state. Capacity is selected by capabilities and resources.

### Fault tolerance layers

1. **Process**: `kiln supervise` restarts the server with jittered exponential backoff and a crash
   log; a healthy period resets the backoff.
2. **Subsystem**: in-process supervisor restarts failed tasks (poller, scheduler, listeners).
3. **Job**: retries, agent-loss requeue, timeouts, cancellation that kills the whole process group.
4. **State**: Amphora owns atomic commit, flush and record replication; recovery reconciles durable intent with runner evidence.

## Agent transport

Agents keep one QUIC connection to the server (hyper-transport, mutual TLS: the agent checks the
server's certificate under the fixed name `kiln-server`, the server the agent's, both against the
kiln CA). **No ports beyond the HTTP one are required; UDP is used when reachable.** An agent dials
the QUIC address over UDP first. When two direct dials in a row get no answer (or direct dials go
unanswered for 20 s) it opens the HTTP tunnel instead: a WebSocket to `GET /api/agents/tunnel` on
the server's HTTP port (`ws://`, or `wss://` when the server URL is `https://`), with a UDP socket
on loopback at its end that its QUIC endpoint dials. Each binary message is one datagram; the server
relays them to and from its own QUIC listener from a loopback socket per tunnel (a wildcard
`quic_addr` such as `0.0.0.0:7071` is reached at `127.0.0.1:7071`, `[::]` at `[::1]`, a specific
address as itself).

```text
  agent QUIC ─UDP─▶ relay 127.0.0.1:<any> ═WS /api/agents/tunnel═▶ server relay ─UDP─▶ QUIC listener
```

QUIC runs end to end through the tunnel unchanged, so the tunnel adds no trust: the route is open at
the HTTP level, and bounded instead (256 tunnels at once, 60 new ones a minute per source address,
messages of at most one 1500-byte datagram, closed after 10 idle minutes). While tunnelled, the
agent checks every 5 minutes whether UDP answers again (a 1200-byte datagram of a reserved QUIC
version, which the listener answers with Version Negotiation, no handshake) and switches back when
it does, closing the tunnel once the direct connection is up. It logs `connected directly` or
`connected through the HTTP tunnel (UDP <addr> unreachable)` each time the transport changes.
The agent names the TLS exporter value of the connection the tunnel carries in a text message
(`bind <hex>`), which only the two ends of that connection can know; the server matches it against
its own connections' to show the transport of each agent (`transport` in `GET /api/agents`).

The tunnel costs throughput, not correctness: datagrams ride one TCP connection, so a lost segment
holds up every QUIC stream behind it (head-of-line blocking) and QUIC's congestion control sees TCP's
delays rather than the path's; terminals and job traffic are small enough not to notice. Datagrams a
full socket cannot take are dropped, as a congested link would, and QUIC recovers them.

### Networks that block UDP or inspect TLS

A company network (Zscaler, a proxy, a VPN) may drop UDP, let traffic out only through a proxy, and
re-sign every HTTPS connection with its own root certificate. A runner there works over TCP alone:

- **The tunnel, at once.** A runner keeps the path it last reached the server by
  (`<state>/transport`): one that last connected through the tunnel takes it first after a restart
  or an upgrade, instead of waiting 20 s for UDP again, and still switches back once UDP answers.
  Everything a runner does with other runners goes through the server (reach, repository sync,
  clones), so it all rides the tunnel too.
- **Proxies.** HTTPS, the tunnel and joining go through `HTTPS_PROXY` / `HTTP_PROXY` / `ALL_PROXY`
  (or their lowercase forms; `http://[user:password@]host:port`), or, on macOS, the system's proxy
  settings when none is set (not a PAC file). Every connection is a `CONNECT` tunnel through it,
  plain HTTP included, so the tunnel's WebSocket gets through. Loopback and private addresses and
  `localhost` / `.local` names always go direct (a proxy on the internet can't reach them), and so
  does whatever `NO_PROXY` or the system's exceptions list.
- **TLS inspection.** Beside the public roots, HTTPS trusts this machine's own: on macOS the
  keychains, on Windows the Root stores, elsewhere the system's CA bundle, and `SSL_CERT_FILE` /
  `SSL_CERT_DIR`. That is where a company installs its inspection root. It changes no trust between
  kiln's ends: QUIC runs end to end inside the tunnel, still pinned to the kiln CA.
- **Joining.** Finding servers on the network needs multicast UDP; with it blocked, join by
  address: `kiln join --server https://kiln.example.com`, or the server's own
  `curl -fsSL https://kiln.example.com/install.sh | sh -s -- --join`.

Where only web ports pass, serve kiln on 443 (`[server] https`, or a front door, docs/front-door.md):
the server's URL then is the only address a runner needs.

## Home network: discovery and joining

kiln instances on one LAN find each other, and any computer can become an agent of a server with
one command and an admin's click. Every machine that joins appears on the server's Agents page
with its terminal, metrics and jobs: one pane of glass across the computers in the house.

```
 other computer (kiln join)                 kiln server                           admin (UI)
 mDNS query _kiln._tcp.local ────────────▶  answers: PTR, SRV, TXT, A
 POST /api/join-requests ────────────────▶  keeps the request (memory, 10 min)
   {name, os, arch, labels, CSR, H(secret)}
                     ◀──────────────────── {id, nonce, CA}
 shows the verification code                Runners → Add machines: code ─────▶ same code?
 GET /api/join-requests/<id>, every 2 s ─▶                                   ◀──── Approve
                     ◀──────────────────── {approved, cert, CA, QUIC}
                                            (the CSR signed by the kiln CA, as in enrollment)
 stores the credentials, installs the agent, connects over QUIC + mTLS like any enrolled agent
```

### Announcing and browsing (multicast DNS)

`crates/kiln/src/dns.rs` is the slice of the DNS wire format multicast DNS needs (A, AAAA, PTR,
SRV, TXT, name compression), decoded defensively: names ≤ 255 bytes, labels ≤ 63, compression
pointers only backwards and at most 32 per name, at most 256 records per message.
`crates/kiln/src/discovery.rs` is the responder and browser, written on hyper-rt UDP (no mDNS
crate, no extra threads or runtime):

* **Sockets.** One per address family on UDP 5353, created with `socket2` (`SO_REUSEADDR` and
  `SO_REUSEPORT`, because avahi or mDNSResponder usually own the port too), joined to
  224.0.0.251 and ff02::fb on every LAN interface, then adopted by hyper-rt. Interfaces are the ones
  that are up and multicast-capable, minus loopback, point-to-point links (VPNs) and container or
  VM bridges (`docker*`, `br-*`, `virbr*`, `cni*`, …); `[discovery] interfaces` overrides the choice.
  Interfaces are re-read every 30 s.
* **What is announced.** `_kiln._tcp.local.` with the instance name `kiln <role> on <host> (<id8>)`,
  an SRV record pointing at the HTTP port (0 for agents) on the host name `kiln-<id8>.local.`, its A
  records (AAAA too when the HTTP server listens on IPv6) and a TXT record:

  | Key | Value |
  | --- | --- |
  | `id` | stable random instance id, kept in `<data dir>/instance.id` (agents: their state dir) |
  | `name` | host name |
  | `role` | `server` or `agent` |
  | `version` | kiln version |
  | `quic` | QUIC port (0 for agents) |
  | `ca` | servers: first 16 hex digits of the SHA-256 of the CA certificate |
  | `enrolled` | agents: the same fingerprint of the CA of the server they are enrolled with |
  | `os`, `arch` | e.g. `linux`, `x86_64` |

  Names derive from the random id, so kiln needs no probing or conflict resolution and never claims
  the host's own `.local` name.
* **When.** At start, one and two seconds later, then every 30 s, separately on each interface with
  that interface's addresses. Queries are answered: legacy queries (from a port other than 5353)
  and QU questions by unicast, the rest by multicast after a 20–120 ms delay, at most once a second
  per interface, with known-answer suppression. Goodbyes (TTL 0) go out when the server stops.
* **Browsing.** The server queries at start and folds every response it hears into a table of
  instances (its own packets are recognised by id and ignored). Records carry a TTL of 120 s, so an
  instance that misses four announcements expires; a goodbye removes it at once. Addresses heard on
  several interfaces are merged and lapse one by one. `GET /api/discovery` lists the table and marks
  each instance that already is one of this server's agents: an agent announcing `enrolled` = this
  server's CA, or a machine whose host name one of the server's agents has.
* **Agents.** `kiln agent --discover` announces the agent (role `agent`) while it runs. When it is
  not enrolled yet, it picks the server it hears (asking when there are several and a terminal is
  attached) and goes through the join flow below.

A machine where port 5353 cannot be shared, or with no multicast network, works on without
discovery and retries every minute. `[discovery]` in the config has `enabled`, `announce`,
`browse`, `join_requests` and `interfaces`; everything is on by default.

### Joining (`kiln join`)

Joining is never automatic: a machine asks, an admin decides.

1. `kiln join` browses for three seconds (`--server URL` skips that), lists the servers it heard
   (asking which one when there are several; `--yes` takes the only one), generates a key and a
   CSR locally (the private key never leaves the machine) and a random secret, and sends
   `POST /api/join-requests` with its name, host name, OS, architecture, labels (default
   `[baremetal, <os>, <arch>]`), the CSR and the SHA-256 of the secret.
2. The server keeps the request in memory for 10 minutes and answers with an id, a random nonce and
   its CA certificate. At most 20 requests wait at once and each source address may send one every
   5 seconds (HTTP 429 with `Retry-After` otherwise).
3. The machine polls `GET /api/join-requests/<id>` every 2 s with the secret as a bearer token. The
   first poll confirms the request: from then on admins see it under **Runners → Add machines**,
   with its address and verification code, and can approve or deny it.
4. **The verification code** is six digits taken from SHA-256 over the CSR's public key, the
   server's CA certificate, the machine's secret and the server's nonce; both sides compute it.
   The machine commits to its secret (sends only its hash) before it sees the nonce, and the server
   chooses the nonce after the CSR is fixed, so neither another machine nor a man in the middle can
   make the two codes agree except by a one-in-a-million guess per (rate-limited) attempt. The admin
   approves only when the UI shows the code the machine shows. The machine also refuses to continue
   if the server reports a different code, a different CA, or a certificate for another key.
5. **Approval** signs the CSR with the kiln CA exactly as enrollment does (365 days). The agent's
   name is the requested one unless an agent on another host (or the server's local agent) already
   has it; then it becomes `<name>-2`, `-3`, …, and the agent takes its name from the certificate.
   The server's own machine asking for its local agent's name is refused (409) rather than named
   `<name>-2`: that agent is already the machine's runner, and a machine runs one.
   The answer carries the certificate, the CA and the QUIC address (the address the machine reached
   the HTTP API at, when that is an IP, else the advertised one).
6. The machine stores the credentials in the agent state directory like enrollment
   (`agent.key`, `agent.pem`, `ca.pem`, `quic`, plus `server` and `labels`) and runs the
   `kiln install --agent` logic, so the agent starts now and on every boot. `--no-install` runs the
   agent in the foreground instead.

A request that vanished (server restart, expiry) is submitted again, with a new code. On a machine
with no server configuration, `kiln up` offers to join a server it hears instead of starting a new
one. Without a terminal, `curl -fsSL http://<server>:7070/install.sh | sh -s -- --join` downloads
the binary from the server and runs `kiln join --server <server>`.

**Firewalls.** Discovery needs UDP 5353 open to the LAN on every machine involved, e.g.
`sudo ufw allow from 192.168.1.0/24 to any port 5353 proto udp`. Joining and running as an agent
need only the server's HTTP port (TCP 7070) reachable from the machine; its QUIC port (UDP 7071) is
used when it is reachable too, and agents tunnel QUIC through the HTTP port when it is not (see
[Agent transport](#agent-transport)).
