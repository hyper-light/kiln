# kiln HTTP / WebSocket API (browser + CLI)

Agents do **not** use this API except for `POST /api/agents/enroll`, the join requests of
`kiln join` and, when UDP to the QUIC port gets no answer, the tunnel `GET /api/agents/tunnel`;
they speak QUIC (see `docs/protocol.md`). All JSON. Timestamps are unix seconds unless
the field ends in `_ms`.

## Auth

Every `/api/*` route except `/api/auth/*`, `/api/agents/enroll`, `/api/agents/tunnel` (the QUIC
mutual TLS inside it authenticates), `POST /api/join-requests`, `GET /api/join-requests/:id` (which
takes the join secret instead) and `/api/hooks/*` requires
either the `kiln_session` cookie or `Authorization: Bearer <admin token>`. 401 → UI shows the login
screen.

| Method | Path | Body / query | Result |
| --- | --- | --- | --- |
| POST | `/api/auth/login` | `{ "token": "…" }` | 204 + cookie |
| GET | `/api/auth/otp?code=…` | one-time code from `kiln login` | 302 → `/` + cookie |
| POST | `/api/auth/logout` | | 204 |
| GET | `/api/me` | | `{ "authenticated": true, "version": "0.2.0", "hostname": "…" }` |
| POST | `/api/me/door` | | the HTTP/3 door's URL with a single-use ticket and the certificate hashes to pin, or `{ "on": false }` (docs/browser-protocol.md, "The HTTP/3 door") |

## Types

```ts
type Status = "pending"|"queued"|"running"|"succeeded"|"failed"|"cancelled"|"skipped"|"timed_out";
interface Repo { id; name /* owner/repo */; clone_url; branches: string[]; poll_seconds; enabled: boolean;
  created_at; last_polled_at: number|null; last_error: string|null }
interface Run { id; repo_id; repo_name; number; pipeline; event /* push|pull_request|tag|manual|schedule */;
  branch; sha; message; author; status: Status; created_at; started_at|null; finished_at|null;
  error: string|null; concurrency_group: string|null }
interface Step { idx; name; status: Status; started_at|null; finished_at|null; exit_code: number|null }
interface Job { id; run_id; key; display; status: Status; labels: string[]; needs: string[] /* job keys */;
  agent: string|null; executor: "baremetal"|"k8s"|null; attempt; queued_at|null; started_at|null;
  finished_at|null; exit_code|null; error|null; peak_rss: number|null /*bytes*/; cpu_seconds: number|null;
  steps: Step[]; spec?: PlannedJob }
interface HostSample { ts_ms; cpu_pct; cpus; load1; mem_total; mem_used; disk_total; disk_used;
  net_rx; net_tx; procs }
interface Agent { name; labels: string[]; slots; hostname; os; arch; version; ephemeral: boolean;
  connected: boolean; last_seen; enrolled_at; running: string[] /* job ids */; busy: number;
  draining: boolean; sample: HostSample|null;
  transport?: "direct"|"tunnel" /* while connected: UDP to the QUIC port, or the HTTP tunnel */ }
interface Child { name; state: "starting"|"running"|"restarting"|"stopped"|"finished"; restarts;
  last_error: string|null; since_unix }
// A kiln instance heard on the local network (multicast DNS, see architecture.md).
interface DiscoveredInstance { id; instance /* DNS-SD name */; name /* host */; role: "server"|"agent";
  version; os; arch; port /* HTTP, 0: none */; quic /* port, 0: none */; ca: string|null /* 16 hex */;
  enrolled: string|null /* CA fingerprint of the server an agent is enrolled with */;
  addresses: string[]; first_seen; last_seen; url: string|null;
  joined: boolean /* already one of this server's agents */; agent: string|null /* which one */;
  same_ca: boolean; join_command: string /* "kiln join --server <this server as it can reach us>" */ }
interface JoinRequest { id; name /* agent name asked for */; hostname; os; arch; labels: string[];
  version; address /* where it came from */; status: "pending"|"approved"|"denied";
  code: string|null /* 6 digits; null until the machine confirmed */; created_at; expires_at;
  agent: string|null /* name given on approval */ }
// A runner's front door: DNS and HTTPS under a name of your own (front-door.md).
interface FrontDoorSettings { domain /* any name: "kiln.dev" */; aliases: string[];
  dns: { enabled; port /* 53 */; mode: "resolver"|"authoritative"; upstream: string[]; addresses: string[] };
  https: { enabled; port /* 443 */; http_port /* 80; 0: no redirect */; upstream /* "": the runner's --server */ };
  cert: { mode: "kiln" /* self-signed */ | "custom" /* your own */ }; mdns: { enabled; name /* "kiln": kiln.local */ } }
interface CertInfo { subject; issuer; names: string[] /* DNS names and IPs */; not_before; not_after;
  sha256 /* hex, of the DER */; server_auth: boolean }
interface FrontDoorListener { state: "off"|"on"|"waiting"|"error"; on: string[] /* where it listens */;
  error: string|null; to: string[] /* HTTPS: upstream; DNS: resolvers */; served; fix: {why, commands: string[]}|null }
interface FrontDoorView { runner; connected: boolean; supported: boolean|null /* null: not known yet */;
  settings: FrontDoorSettings; applied: boolean|null /* the runner runs these settings */; status_at: number|null;
  status: null | { standby: boolean; os; server; addresses: string[]; dns, https, http, mdns: FrontDoorListener;
    cert: { mode; state; info: CertInfo|null; kiln: boolean /* issued by kiln's root */; missing: string[];
      error: string|null; renewing: boolean }; fixes: {why, commands: string[]}[] };
  origins: string[] /* https://kiln.dev … */; ca: { subject; sha256; not_after }|null /* kiln's root */ }
```

## REST

| Method | Path | Notes |
| --- | --- | --- |
| GET | `/api/overview` | `{ stats, queue: {queued, running}, agents: {connected, total, slots, busy}, system: {version, uptime_s, hostname, generation, children: Child[], sample: HostSample} }` where `stats = { runs_24h, runs_failed_24h, runs_succeeded_24h, median_job_seconds, jobs_by_status: Record<Status,number>, hourly: {ts, succeeded, failed, avg_seconds}[] }` |
| GET | `/api/repos` | `Repo[]` |
| POST | `/api/repos` | `{ name, clone_url?, branches?, poll_seconds? }` → `Repo` |
| DELETE | `/api/repos/:id` | |
| POST | `/api/repos/:id/poll` | poll now |
| POST | `/api/repos/:id/trigger` | `{ branch?, pipeline? }` → `Run[]` |
| GET | `/api/places` | `{ repos: RepoRow[], machines: PlaceMachine[], tasks: PlaceTask[] }`: every repository (built, synced, on machines) with the folders it is in; see [sync.md](sync.md#repositories-on-machines) |
| POST | `/api/places/{clone,sync,move,remove}` | `{ key, from?, branch?, to?: [{machine, path}], delete? }` → `PlaceTask[]` (one per machine) |
| POST | `/api/places/look` | every machine looks at its repositories again → `{ asked }` |
| POST | `/api/places/tasks/:id/retry` | → `PlaceTask[]` |
| DELETE | `/api/places/tasks/:id` | forget a task that is over |
| GET | `/api/runs?repo=&limit=50&before=` | `Run[]` newest first |
| GET | `/api/runs/:id` | `{ run: Run, jobs: Job[] }` |
| POST | `/api/runs/:id/cancel` | `{ jobs?: [ids] }`: the run, or just those of its jobs, the run going on → 200 `{ run, jobs: [{ job, cancelled, why? }] }` (W26-R21); without `jobs` → 204 |
| POST | `/api/runs/:id/retry` | `{ only_failed: bool, jobs?: [ids] }`: with `jobs`, just those, passed ones too, each with the jobs that need it → 200 `{ kind, run, jobs: [{ job, again, attempt?, waiting?, why? }] }` (W26-R21) |
| POST | `/api/runs/:id/pause` | `{ jobs: [ids] }`, either engine's run: a running job frozen where it runs, its time limit stopped; a waiting one held, dispatched nowhere → 200 `{ run, jobs: [{ job, paused, why? }] }`. A runner that can't freeze (`CAP_FREEZE`) is refused saying so; without `jobs` → 400 (`runs.pause`, W26-R22) |
| POST | `/api/runs/:id/resume` | `{ jobs: [ids] }`: those paused go on from where they were → 200 `{ run, jobs: [{ job, resumed, why? }] }` (`runs.pause`, W26-R22) |
| GET | `/api/ci/legs/:id/runners` | where a finished job could run again: `{ kind, ran_on, elsewhere, retry: { can, why }, runners: [{ name, connected, draining, busy, slots, able, why }] }` ([ci.md](ci.md) W05-R28) |
| POST | `/api/ci/legs/:id/retry` | `{ runner?, elsewhere? }`: a finished job again, on any runner that can run it, on any but the one it ran on, or on `runner` → 201 (a pipeline's job: its next attempt in its run, with the jobs that need it) |
| GET | `/api/jobs/:id` | `Job` with `spec` (and `placement`, `{ runner }` or `{ avoid }`, while a retry placed it) |
| GET | `/api/jobs/:id/logs?step=N` | raw bytes (ANSI) of that step |
| GET | `/api/jobs/:id/logs/times?step=N` | `{ times: [[offset, unix_ms], ...], size }`: when each piece of that step's log arrived, by its byte offset, oldest first (none for a log written before kiln noted them), and how long the log was as they were read |
| GET | `/api/jobs/:id/tools` | `{ job, missing: { tools, on, why, no_agent, runners: [{ runner, unmet, tools, fixable, no_agent }] }, fix }`: what the job lacks to run, from its failed step's log (`tools`, `on` the runner, `why` the line) or, waiting on its `requires`, from each runner's facts; why no agent can install it there (`no_agent`: a pod, a runner away or without terminals); and the agent installing it (see [pipelines.md](pipelines.md#runners)) |
| GET | `/api/ship/commits?repo=&host=&kind=&lens=&limit=&cursor=&by=` | Ship's list: commits newest first, a page at a time → `{ commits: [row], next_cursor, pace? }`. Each row's `ran` (`{ from, to }`, unix seconds, `to` null while a job runs; null before any starts) and `machines` (in the order its jobs started) say when and where its jobs ran. `by=repo`: Ship's rows, each repository's newest commit, newest first (ci.md W26-R29). `viewer` (the caller's principals the rows were picked by, against a `ship` frame's `whose`), on a page memory answered, says `ship` frames say how it changes; a page without it loads again (`runs.read`) |
| POST | `/api/ship/hand-off` | `{ repo, sha, hand_on?, unsigned? }` → `{ handed: { to, agent, incident?, what }, custody }`: Ship's Hand off (ci.md W26-R18). What the commit has open goes to one agent: its change's author's agent, while it runs, gets the change's conflicts, open threads and failed checks; a failure with no agent of its own goes to the agent attached to what failed (an incident). The row then says who is on it. 409 says why when there's nothing open or no agent to take it (`runs.rerun` on its repository). The person's side is signed (ci.md W27-R08): `hand_on` is their own hand-on, signed on their device, to the agent or, from a browser, to kiln's server, which passes it on to the agent; without it, kiln's take for them says their signature is missing and why (`unsigned`: their browser's reason). `custody` is `{ hand_on, passed_on?, kiln_signed_for_you }` |
| POST | `/api/ship/hand-off/statement` | `{ repo, sha }` → `{ payload_type, statement, payload, pae, required }`: what a person signs to hand the commit off from a browser, with the browser's own key, said as a verdict's is (ci.md W27-R08): their hand-on of what's open of it to kiln's server. Refused as Hand off would be (`runs.rerun` on its repository) |
| POST | `/api/ship/jobs` | `{ repo, sha, verb: retry\|pause\|resume\|cancel, jobs?: [ids], scope?: failed\|running\|waiting\|all }`: a commit's jobs acted on together, across its runs, each run's own call underneath → 200 `{ repo, sha, verb, jobs: [{ job, run?, done, why? }] }`; the verb's right on the repository (`runs.rerun`, `runs.pause`, `runs.cancel`) or 403 (W26-R21, R22). `GET /api/ship/commits/:repo/:sha` lists the commit's `jobs`, each with its `verbs` and `why_not`, with `dv` (the version of what it shows beyond its row) and, from memory, `viewer` |
| GET | `/api/ship/deploys?q=` | Ship's Deploys (ci.md W26-R24): every KilnCD app by project → `{ projects: [{ project, say, needs, apps: [row] }], total, at_ms }`. A project is the repository its app's deploy file is in or its manifests come from (`owner/name`), else its Argo CD project; projects and their apps come what needs someone first (a rollout waiting on a person, then what failed, then what went back, then what's out of sync for a person to sync), then by name, each heading saying how many and how many stand as its most pressing (`2 apps · 1 waits on you`). A row: `{ app, name, project, environment, cluster, namespace, stand, word, line, pips: [{ stage: sync\|health\|soak, state, p? }], by, at, runs, sync, health, auto_sync, pinned, v }`, where it stands as `kiln_cd::stands` says, of its latest deploy; `q` finds by project, app, environment, cluster or namespace. Held in memory as Ship's commits are, and gathered again once CD's events (`cd_apps`, `cd_rollout`, `cd_app_deleted`) or its store's changes say something changed, or Ship acted (else at most 30 s later); where each stands is worked out at each read. Read with `runs.read` on the whole server, as `/api/cd`'s reads are; the page asks again on `cd_apps` and `cd_rollout` |
| GET | `/api/ship/deploys/:app` | An app's pane (`:app` its id, two segments: `default/web`) → `{ app: row, about: { cluster, namespace, objects, source, repo }, groups: [{ name, note, owner?, items: [{ key, group, kind, namespace, name, state, v, logs, differs, node?, container? }] }], tree: { live, errors }, rollout: { id, revision, state, note, steps: [{ n, state, what, v, detail? }], under_way, waits, strategy, said, promote_to, approvals, deploy, back_to, weight, sides: [{ side: stable\|canary, name, pods, share, up }] }?, history: [{ deploy?, sha?, short?, subject, by?, at_ms, state, now, dim, to?, revision?, same_as? }], settings: [{ key: auto_sync\|auto_rollback\|prune\|self_heal, on, from, what, can }], why_not: { verb: why } }`: its components each workload's group (its pods, uptime and restarts, and ReplicaSets under it) and then by what they are, read live from its cluster (else what KilnCD holds, for a target beyond Kubernetes or a cluster not reached); a rollout's steps; its history, its source's newest commits each as the deploy that ran it or else one a rollback can go to (`revision`), then older deploys (`to`); and why each strip verb can't apply, where one can't (`delete` where something else declares the app). A pod's item has its `node` and the `container` its logs read; a rollout, its step under way's analysis as measured (`detail`: each check's latest measurement), its strategy (`canary`, `blue/green`), the share Promote moves it to (`promote_to`), the deploy it rolls out, the one it goes back to and the new one's share of traffic (`weight`, %), and on a cluster its two sides: its workload's ReplicaSet that ran (`stable`) and the newest (`canary`), each with its pods and share. A sync under way's line names the deploy it will be (`#42 · wave 2 of 3`). An item's `key` is the `:key` of `/api/cd/apps/:app/resources/:key` (logs, events, manifest, diff) |
| POST | `/api/ship/deploys` | `{ app, verb, … }`: a strip verb on an app, each its CD operation, only what that takes passed on: `sync` (`resources`: picked keys or `{group, kind, namespace, name}`; `prune`, `dry_run`, `options`) → `cd.apps.sync`; `refresh` (`hard`) → `cd.apps.refresh`; `rollback` (`to` a deployment, or `revision`: any commit, W26-R25) → `cd.apps.rollback`; `stop` → `cd.apps.terminate`; `follow` → `cd.apps.follow`; `promote` (`full`), `abort` (`reason`), `approve` and `deny` (`check`), with `rollout` → `cd.apps.rollout_act`; `auto_sync` and `auto_rollback` (`on`: true, false or null) → `cd.apps.set_auto_sync`, `cd.apps.set_auto_rollback` (W26-R27); `delete` (`cascade`: `foreground` or `orphan`, which leaves its objects) → `cd.apps.delete`, or 409 for an app its repository's `.kiln/deploy.yaml` or another app declares, as it would come back → 200 `{ app, verb, op, done }`, `done` what the operation answered. As the operation's route would let the caller (`environments.deploy` on the whole server, or 403; an agent only where its environment lets agents deploy), its input checked as its type, and audited in this request's one row; what the operation refuses comes back as it says it (W26-R24) |
| POST | `/api/jobs/:id/tools/fix` | `{ runner?, kind?, mode? }` → `{ fix }`: an agent, as the caller, installs what the job lacks on that runner; then kiln probes the runner's facts and runs the run's failed jobs again (`runs.rerun` on its repository, and `agents.run` as for any agent) |
| GET | `/api/agents?lean=` | `Agent[]`; with `lean=1`, each with only its facts' `virt` of them (whether it starts microVMs): what a page that lists the runners and starts agents on them reads, a third of the list (Mission control as it opens; the Runners page reads it whole) |
| DELETE | `/api/agents/:name` | forget agent |
| POST | `/api/agents/:name/drain` | `{ draining: bool }` |
| GET | `/api/agents/:name/diagnose` | what's wrong with that runner, worst first, each with its repair ([releases.md](releases.md#diagnosing-and-repairing-a-runner)) |
| POST | `/api/agents/:name/repair` | `{ action, way? }`: a repair a diagnosis offers |
| GET | `/api/agents/join` | `{ token, server, quic, command, manual, join, join_manual }` – bare-metal join commands: with the join token (`command`, `manual`), or asking for approval (`join`: `install.sh --join`, `join_manual`: `kiln join --server …`) |
| GET | `/api/runners/:name/agent-limit` | `{ runner, own, connected, runs_agents, supported, max_sessions, held: { max_sessions, from: "server" \| "config" \| "default", config? } \| null, working, waiting, sessions, default, max }`: the most agent sessions the runner has at work at once, what's set here (`max_sessions`, null for none), what it holds and why as it last said, how many of its sessions are at work now (`working`, what the limit counts) and wait for room to go to work (`waiting`; both null while it's offline), and how many it has in all (`sessions`, finished, paused and ended ones too) ([agents.md](agents.md#how-many-agents-a-machine-runs)); 404 for an unknown runner |
| PUT | `/api/runners/:name/agent-limit` | `{ max_sessions: n }` (1 to 4096), or `{ max_sessions: null }` to take it away (its config's, or kiln's default of 64, holds again) → the same view. Wins over the runner's config; stored and pushed to the runner (held when it next connects if it's offline). 400 for anything else, saying why; audited `agent_limit.update` |
| GET | `/api/runners/:name/engine` | `{ runner, engine: "host" \| "microvm" \| null, from: "server" \| "default" \| null, why, asked, version, microvm: { usable, why } \| null, isolation, virt, sessions, draining }`: how the runner runs new agent sessions (its engine), where that comes from (its owner set it, or what it can do: `microvm` where it can hold microVMs, else `host`) and why, what's set here (`asked`, null for none) and how many times it was set (`version`, which a start placed under an older one is refused by), whether it can hold microVMs now and why not, its isolation facts, and how many of its running sessions are on its engine (`sessions`) and on the other (`draining`, which finish there), each of those (`draining_sessions: { id, name, kind, state, engine, created_at }[]`, those an agent started before it), and while any drains the switch they drain for (`switch: { from, to, at } \| null`, `from` and `to` as `asked` says them); null fields until its facts come ([isolation.md](isolation.md#a-runners-engine)); 404 for an unknown runner |
| POST | `/api/runners/:name/engine/end` | "End them now": each draining session ended, after the agents it started → the same view, with `ended: string[]` (the sessions ended). `runners.manage`; destructive (`runners.engine.end_draining`: an agent's call waits for its person's click); audited `runner_engine.end_draining` |
| POST | `/api/runners/:name/engine/cancel` | its engine set back to `switch.from` while what the switch drains runs, as a `PUT` sets it (a new `version`) → the same view; 409 where it wasn't switched, or nothing drains (`runners.engine.cancel`) |
| PUT | `/api/runners/:name/engine` | `{ engine: "host" \| "microvm" }`, or `{ engine: null }` for what the runner can do → the same view. `microvm` is refused (409), saying why, where the runner can't hold microVMs now. Stored, held in the server's facts of it at once (`isolation.engine`, `virt.microvm` only on `microvm`) and pushed to the runner; nothing running stops. 400 for anything else, saying why; audited `runner_engine.update` |
| GET | `/api/front-doors` | `FrontDoorView[]`: every runner with front-door settings (see [front-door.md](front-door.md)) |
| GET | `/api/runners/:name/front-door` | `FrontDoorView`; 404 for an unknown runner |
| PUT | `/api/runners/:name/front-door` | `FrontDoorSettings` → `FrontDoorView`. Checked, stored and pushed to the runner (applied when it next connects if it is offline); 400 with the reason (`Domain: kiln needs a dot, like kiln.dev`); audited `front_door.update` |
| POST | `/api/runners/:name/front-door/cert` | `{ cert_pem, key_pem }` (≤ 256 KiB) → `{ cert: CertInfo, front_door: FrontDoorView }`: your own certificate, handed to the runner, which keeps it; sets `cert.mode` to `custom`. 400 when it doesn't parse, the key isn't the certificate's, it isn't for TLS servers (no serverAuth) or it has expired; 409 while the runner is offline. The server keeps neither and never sends the key back; audited `front_door.cert` |
| GET | `/api/runners/:name/front-door/cert` | the certificate chain the runner's HTTPS serves, as it last reported it: `application/x-pem-file`, `attachment; filename="<first name>.pem"`, certificates only (never a key); 404 when it serves none |
| POST | `/api/runners/:name/front-door/cert/renew` | renew the self-signed certificate now → `FrontDoorView`: the runner makes a new key and asks at once, and serves the old certificate until the new one is in (`status.cert.renewing` meanwhile). 409 for your own certificate, with HTTPS off, or while the runner is offline; audited `front_door.renew` |
| GET | `/ca.pem`, `/ca.crt`, `/ca.mobileconfig` | **open** · kiln's root (the CA that signs self-signed front-door certificates): PEM, DER, and an Apple configuration profile carrying it as a root |
| GET | `/api/discovery` | `{ enabled, announce, browse, join_requests, self: {id, name, instance, ca, port, quic, version}, instances: DiscoveredInstance[] }` |
| POST | `/api/join-requests` | **open** · `{ name, hostname, os, arch, labels?, version?, csr /* PEM */, commitment /* sha256 hex of the secret */ }` → 201 `{ id, nonce /* hex */, ca_pem, expires_in, server, version }`; 400 bad CSR/commitment; 429 + `Retry-After` (≤ 20 waiting, 1 per address per 5 s); 404 when `[discovery] join_requests = false` |
| GET | `/api/join-requests/:id` | **open** with `Authorization: Bearer <secret>` → `{ id, status, code, expires_in, approval: null \| { name, labels, cert_pem, ca_pem, quic, quic_port } }`. The first poll confirms the request (its code becomes visible to admins). 403 wrong secret, 404 unknown or expired. With admin auth: the `JoinRequest` |
| GET | `/api/join-requests` | `JoinRequest[]` (all live ones, oldest first; requests live 10 minutes) |
| POST | `/api/join-requests/:id/approve` | optional `{ name?, labels? }` → `{ id, status: "approved", name, labels }`. Signs the CSR with the kiln CA like enrollment. 409 when already decided or not confirmed yet |
| POST | `/api/join-requests/:id/deny` | → `{ id, status: "denied", name }`; the machine learns it on its next poll |
| GET | `/api/metrics/history?agent=` | `{ [agent]: HostSample[] }` last hour, 5 s resolution (`agent` optional; server is `"server"`) |
| GET | `/api/system` | `{ version, uptime_s, generation, children: Child[], crash_log: string[], config: object }` |
| GET | `/api/conversations/status` | every conversation store as the server sees it: `{ at, on, mirrors_wanted, restore, stores: ConvoStore[], notes }` (docs/conversations.md, "Telemetry") |
| GET | `/api/conversations/series?key=&window=` | one store's series, a column each (`t` in unix s; rates per second): an hour at 5 s, up to a day at a minute |
| GET | `/api/audit?limit=` | `{ts, actor, action, detail}[]` |
| GET | `/api/push` | `{ enabled, available, why, public_key, kinds, muted, reached }`: whether this server pushes notifications to devices, and the key to subscribe with ([notifications.md](notifications.md#api)) |
| GET, POST | `/api/push/subscriptions` | your devices; a browser's subscription with its name and what it hears (at most 20 devices each) |
| DELETE | `/api/push/subscriptions/:id` | forgets one of your devices |
| POST | `/api/push/test` | `{ id? }`: a test to your devices → `{ sent, results }` |
| PUT, DELETE | `/api/push/mutes/:agent` | mutes or unmutes an agent's notifications for you: `push.mute`, `push.unmute` |
| GET | `/sw.js`, `/manifest.webmanifest` | **open** · the service worker that shows pushes (no fetch handler: it never intercepts the app), and the web app manifest |
| GET | `/api/voice` | `{ transcribe, language }`: whether kiln transcribes recordings ([voice.md](voice.md)) |
| POST | `/api/voice/transcribe?lang=` | body: the recording (`Content-Type: audio/*`, up to 25 MiB) → `{ text }`; 501 not set up, 415 not a recording, 413 too big, 502 the service failed |
| POST | `/api/hooks/github` | GitHub webhook (HMAC `X-Hub-Signature-256`) → immediate poll |
| GET | `/metrics` | Prometheus exposition |
| GET | `/healthz`, `/readyz` | liveness / readiness |

## Agentfiles

An Agentfile is Shards' image of an agent's microVM: a Dockerfile's instructions, and `AGENT`,
`HARNESS`, `SKILL`, `MCP`, `NETWORK`, `CONNECT`, `ATTACH` and `EXPOSE … FOR`. People keep their own
in kiln, each with every version of its text (`crate::agentfiles`). The routes need `agents.run`; each
keeps a person to their own, and answers 404 for another's. The server's owner, and whoever has
`agentfiles.manage` (an admin), reach everyone's; an agent reaches those of the person it acts for.
Each route is an operation too (`agentfiles.*`: `POST /api/ops/<name>`, kiln's MCP tools
`agentfiles_*`, code mode's `kiln.agentfiles.*`), and each change is audited.

Kept, a text is checked by Shards itself: Shards' own Agentfile parser and build checks, in the
server's process (`shards-agentfile`, kiln's vendored Shards; docs/shards.md), as `shards build
--call=check,format=json` checks it (BuildKit's lint subrequest as Shards ports it), with a target no
stage can be named, so that its planning stops before it resolves any base: the text parsed, its
instructions and directives read, its names and grants checked (`CONNECT`, `ATTACH`, `FOR`, `EXPOSE …
FOR`), its stages linted. No base image is pulled and no registry is asked. A text Shards refuses isn't
kept: 422, `Shards refuses this Agentfile: <Shards' words>`. Every text is checked; `check` says
`checked: false`, and why, only where the check itself never ended.

A text is kept in kiln's store of bytes (docs/blobs.md) at `agentfiles/sha256/<its SHA-256>`, once for
every Agentfile and version that has it, whoever's; kiln.db keeps the records, each naming its text
by that digest. The text is kept first and the record after it, so a store that can't keep a text
leaves nothing that names it: 503, in the store's words, as when it can't give one back or gives back
bytes that aren't the text its digest names. Deleting an Agentfile lets go of its texts that no other
Agentfile or version names. Its types are generated from the server's (`ui/src/api/wire`):

```ts
interface Agentfile { id; owner /* "owner" | "user:<id>" */; name; text; version; created_at; updated_at }
type AgentfileBrief = Omit<Agentfile, "text">;
interface AgentfileVersion { version; text; at; by /* as the audit trail names them */ }
interface AgentfileCheck { checked: boolean;
  unchecked?: { reason: "no_shards" } | { reason: "timed_out" } | { reason: "failed"; status: number|null; said: string };
  diagnostics: { code /* the rule, or "shards" for the error */; severity: "error"|"warning"; message; path: "Agentfile";
    range: { start: { line; col }; end: { line; col } }; url?: string }[] }
interface AgentfileSaved { agentfile: Agentfile; check: AgentfileCheck }
interface AgentfileListing { agentfiles: AgentfileBrief[]; reaches_all: boolean /* may ask for `all` */; builtin: { name; harness; text }[];
  project?: { repo; sha: string|null; agentfiles: { name; path; text: string|null }[] } }
```

## Confirming an agent's act

An agent can manage its person's agents from their chat, and nothing it reads can make it act for them
unasked (kiln plan 01a1dd2e8618733a87f2793aab1322a8 P5.6; `crate::confirms`). A person's message to an
agent, or goals they give it, makes the turn it starts theirs, until that turn ends; kiln records it where
it records a person's command, by every path, and an agent's own words never count. Every operation an
agent calls passes one gate, before any of it runs (`crate::ops::call`, so CI's, the hosts',
experiments', Ship's and deployments' operations as much as kiln's own):

- At once: reads; its acts on itself and on what it started (its children and theirs, the plan runs they
  ran); messages and questions to its person's other agents, as their own rules have them; and every
  other change it may already make.
- Waiting on its person's click: an operation the registry marks destructive (`destructive`, enforced
  here, not only a hint to MCP clients); a change to another of its person's agents it didn't start
  (interrupting, ending or resuming it); and any change to their agent definitions. Asked in a person's
  turn, kiln keeps the ask (the operation, its exact arguments, its target's version then: a definition's
  version, a session's status and newest event) and answers **428**, `waiting on <person>'s click`. Asked
  outside a person's turn, there's no one to ask: 403, and today's rules stand.
- Another person's agents are out of reach either way, as they are today.

| Method | Path | |
|---|---|---|
| GET | `/api/confirms?agent=` | `{ asked: { id, agent, op, says, state, asked_at }[] }`: what waits on your click, newest first |
| POST | `/api/confirms/{id}` | `{ yes }` → `{ id, state, told }`: your click; the agent is told either way |

Both are a person's: an agent's call is refused. Confirmed, the agent's same call (the same operation and
arguments, its target unchanged) runs once, as the person whose click it was, with their grants and their
reach over their own agents, and its audit row says whose click and which ask. Another call, a second
one, or one whose target changed since (409, and the next such call asks anew) doesn't run. A
confirmation lasts until the person's next message to that agent, which drops what was asked before it;
a click after the agent's turn ended still confirms. kiln keeps the confirmation and never hands it to
the agent, so there's nothing for an agent or a page it reads to copy, guess or replay. An op an agent
makes in a person's turn says so on its audit row, whichever operation it is. The Agents page's Remove is
`agents.remove` (`DELETE /api/harness/agents/{agent}`), held to this gate as every operation is.

## Agent definitions

An agent definition is an agent as data (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d P5.1): one of
kiln's ten harnesses (claude, codex, devin, gemini, opencode, goose, amp, antigravity, cursor, pi) and
its model, its instructions and playbook, its tools and MCP servers, its skills, its limits (time,
cost, tokens), and the Agentfile its microVM is made from (one of its owner's, `agentfile:<id>`, or
kiln's for its harness, `harness-<harness>`; none: its harness's own). People keep their own in kiln
(`crate::agentdefs`), each with every version of its settings: a change of settings is a new version,
and so is a restore; a rename alone is none. The routes need `agents.run`; each keeps a person to
their own, and answers 404 for another's. The server's owner, and whoever has `agentdefs.manage` (an
admin), reach everyone's; an agent reaches those of the person it acts for. Each route is an
operation too (`agentdefs.*`: `POST /api/ops/<name>`, kiln's MCP tools `agentdefs_*`, code mode's
`kiln.agentdefs.*`), and each change is audited. (`agents.*` and `/api/agents` are the agents kiln
runs and its runners: a definition's run is one of those agents.)

| Method | Path | |
|---|---|---|
| GET | `/api/agentdefs?all=` | `{ agents: AgentDefBrief[], reaches_all, builtin: KilnAgentDef[], uses: { [kiln_id]: id } }`, newest change first |
| POST | `/api/agentdefs` | `{ name, harness, model?, instructions?, tools?, mcp?, skills?, limits?, agentfile? }` → `AgentDef` (version 1); 422 `kiln can't run this agent: <why>`, 409 a name taken |
| POST | `/api/agentdefs/check` | the settings → `{ ok, problems: string[] }`, keeping nothing |
| GET | `/api/agentdefs/{id}` | `AgentDef` |
| PATCH | `/api/agentdefs/{id}` | what's given changes (null `model` or `agentfile`: back to its harness's), over `If-Match: "<version>"` or `base` (the version it's made over, as you read it) → `AgentDef`; 409 `{ error, base, current: AgentDef }` where another change moved it on from that version (nothing is written) |
| GET | `/api/agentdefs/{id}/versions` | `{ versions: { version, at, by, ...settings }[] }`, newest first |
| POST | `/api/agentdefs/{id}/restore` | `{ version }` → `AgentDef`, that version's settings as a new version |
| DELETE | `/api/agentdefs/{id}` | `{ deleted: true }` |
| POST | `/api/agentdefs/{id}/run` | `{ prompt?, on?, cwd?, name? }` → as `agents.start` answers, with `agentdef: { id, version }`; `id` one of yours or kiln's |
| GET | `/api/agentdefs/{id}/sessions` | `{ sessions: { id, version, at, runner, state, engine, agent }[], starts: { n, by_kind: { [kind]: { n, timed, p50_ms } }, by_start: { [start]: { n, timed, p50_us, from: { [word]: n } } }, warm } }`: the sessions you started from it (`id` one of yours, or kiln's `kiln:<name>`), newest first, each with the version it started from, its `state` (as agents.list's `stand`; `gone` once kiln no longer has it), runner and engine; and what their starts took, by how each started: `by_kind` by engine (`host`, `microvm`), each session's first start timed from its open to its harness running; `by_start` by how each microVM started, where Hex said (`cold`: its guest booted, Shards' `booted`, `saved` or `replaced`; `template`: `restored`; `warm`; `swapped`: a harness switched into its running microVM; a word kiln doesn't know as its own), each timed from its launch to its member running, with the words each kind counts |
| GET | `/api/agentdef-sessions` | `{ [id]: { sessions, starts } }`: every definition's whose sessions you see (yours, everyone's for the server's owner and those with agentdefs.manage, and kiln's own as you started them), by its id, each as `/api/agentdefs/{id}/sessions` answers it, in one read; a definition with none isn't among them |
| POST | `/api/agentdefs/{id}/copy` | `{ name? }` → `AgentDef`: one of kiln's (`kiln:<name>`) or yours, made yours, version 1 |
| POST | `/api/agentdefs/use` | `{ builtin, id }` → `{ uses }`: your own `id` stands in for kiln's UI agent `builtin` for you (null `id`: kiln's own again) |

Kept and restored settings are checked first, and refused, every reason said, where: the harness
isn't one of kiln's ten; the Agentfile isn't one its owner reaches (another person's is as none) or is
kiln's for another harness; the model isn't one kiln's machines list for that harness (what its
sessions said they offer; a harness no machine lists models for takes any); an MCP server has no name
of its own, or not exactly one of `url` and `command`; a tool or skill has no name; a limit is 0.
kiln ships its own, read only, listed as `builtin` (kiln plan 01a1dd2e8618733a87f2793aab1322a8
P5.1b), each made from what kiln itself has so that none drifts from it: one per CI skill
(`kiln-ci-*`: the skill its own, its description its instructions), one per harness (`harness-<id>`,
in kiln's Agentfile for it), and the agents kiln starts from its UI, their briefs shown with what kiln
fills in at each start `<like this>`: `kiln-about` (the card that answers "ask about this code" in an
agent's conversation), `kiln-about-code` (the same in a change no agent made), `kiln-attach` (an
attachment's agent, woken by an incident), `kiln-review` (a review agent) and `kiln-toolfix` (the
agent that installs a tool a job lacks on a runner). Changing, restoring or deleting one of kiln's is
refused, 403: `copy` makes it yours. Each UI agent is `replaceable`: its `brief` is what kiln tells
it at each start, after the instructions of whichever definition starts (kiln's own have none). A
person's own definition may stand in for one, for them alone (`use`): where kiln starts that agent
for them, it starts theirs, its harness, model and Agentfile, first told its instructions and then
kiln's brief. The most specific wins: a harness set where it's started (an attachment's `harness.kind`
its person set, a tool fix's `kind`; left unset, it's stored unset), then the person's own, then
kiln's. An attachment's agent and a review agent are their attachment person's (`by`). Deleting the
definition makes it kiln's own again.

A run starts an agent as `agents.start` starts one, through the one start path (the caller's right
to start agents there checked, and the start audited): the definition's harness and model, `isolation`
`microvm`, `image` its Agentfile (left out: the start's own for its harness), and as its first
message its instructions, then the run's `prompt`.

Each session kiln opens from a definition keeps which, and at which version (kiln's own: 0), kiln's to
say and never what was asked: a run's, and one of kiln's UI agents started from the person's own
definition standing in for it or from kiln's. `agents.get` says it (`definition: { id, version, name,
current }`, `current` the definition's version now: a session behind it has a lesser `version`), and
`sessions` lists a definition's. Their starts are read off kiln's record of each start of a session's
harness on its machine (every one recorded, of the sessions kiln still has): by how each started, which
that record says by its engine (`host`, `microvm`), and, for a session's first start, the time from its
open on its machine to its harness running, room waits and image builds included. Deleting the
definition drops what its sessions say of it.

```ts
interface Mcp { name; url?; command?: string[] }
interface Limits { seconds?; cents?; tokens? }
interface AgentDef { id; owner /* "owner" | "user:<id>" */; name; version; created_at; updated_at;
  harness; model: string|null; instructions; tools: string[]; mcp: Mcp[]; skills: string[]; limits: Limits; agentfile: string|null }
type AgentDefBrief = Pick<AgentDef, "id"|"owner"|"name"|"harness"|"model"|"agentfile"|"version"|"created_at"|"updated_at">;
interface KilnAgentDef { id /* "kiln:<name>" */; name; about; replaceable: boolean; brief?: string;
  harness; model: string|null; instructions; tools: string[]; mcp: Mcp[]; skills: string[]; limits: Limits; agentfile: string|null }
```

| Method | Path | Notes |
| --- | --- | --- |
| GET | `/api/agentfiles?repo=&all=&lean=` | `AgentfileListing`: yours, newest change first, without their texts; beside them, read only, those kiln ships per harness (`builtin`), and with `repo` (`owner/name`, a repository whose runs you read, else 404) its `.kiln/images/<name>/Agentfile` files at its default branch (`text` null past 2 MiB). `all=true`: everyone's (403 without `agentfiles.manage`) With `lean=1` those kiln ships come by their names and harnesses alone, their texts left empty (a start's list, which shows no more of them: the texts are most of what the list carries). |
| POST | `/api/agentfiles` | `{ name, text }` → `AgentfileSaved`, version 1. `name`: an image's (lowercase letters and digits joined by `.`, `_` or `-`, 255 at most), once among yours (409). `text`: 2 MiB at the most (400); Shards' refusal is 422 |
| POST | `/api/agentfiles/check` | `{ text }` → `AgentfileCheck`: what keeping it would say, nothing kept (the editor's lint) |
| GET | `/api/agentfiles/:id` | `Agentfile` |
| PATCH | `/api/agentfiles/:id` | `{ name?, text? }` → `AgentfileSaved`: a new text is a new version (checked as above); a new name alone is none. 409 when another change came first, or the name is one of yours |
| GET | `/api/agentfiles/:id/versions` | `{ versions: AgentfileVersion[] }`, newest first |
| POST | `/api/agentfiles/:id/restore` | `{ version }` → `AgentfileSaved`: that version's text current again, as a new version |
| DELETE | `/api/agentfiles/:id` | `{ deleted: true }`: it and its versions |

An agent starts from one in a microVM of its own (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step
31): `POST /api/harness/sessions` with `spec.isolation: { kind: "microvm", image, cpus?, memory? }`,
`image` naming one kiln ships by its name (`harness-claude`: an image's, so never with a `:`) or one
of yours by its id (`agentfile:<id>`), at its latest version as the agent starts, and `cpus` (CPUs)
and `memory` (bytes) its microVM's size. Another person's is refused, whoever asks (400,
`agentfile:<id> names no Agentfile of yours`). Such a start goes only to a runner that says it
starts microVMs, and is refused anywhere else (400, `an agent can't run isolated there: …`); there
kiln's balancer places it, asking that size, and refuses it where it can't (step 32). The console
keeps them at `/agents/agentfiles` and chooses one as an agent starts (docs/agents.md, "From an
Agentfile, in a microVM of its own").

## Your secrets

A person's own secrets (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, decision 6): the `user` scope of
CI's secrets (docs/ci.md W09), sealed as every scope's are (`crate::secrets::mine`). Each person sets,
lists and deletes their own; no one else sees them, by name or value: not an admin, not
`secrets.manage` (`/api/ci/secrets` refuses `scope=user:…` for everyone, 400), not the server's
owner, whose admin token reaches the owner's own alone. No answer carries a value. An agent lists its
person's names, to name them in an Agentfile, and sets none (403). Each route is an operation too
(`secrets.mine.*`: `POST /api/ops/<name>`, kiln's MCP tools `secrets_mine_*`, code mode's
`kiln.secrets.mine.*`), and each change is audited, by name.

| Method | Path | |
|---|---|---|
| GET | `/api/me/secrets` | `{ secrets: { name, size, created_at, updated_at, last_used_at }[] }`: yours |
| PUT | `/api/me/secrets/<name>` | `{ value }` → `{ set: true, name }`. `name`: letters, digits and `_`, not starting with a digit (kept upper-case); `value`: 64 KiB at the most (422) |
| DELETE | `/api/me/secrets/<name>` | `{ deleted: true }`; 404 for a name you hold none of |

One of your own Agentfiles names each secret a build of it is given, by reference:
`LABEL kiln.secret.<ID>=user:<NAME>`, `<ID>` the build secret Shards reads (`GIT_AUTH_TOKEN`,
`GIT_AUTH_TOKEN.<host>`, `HTTP_AUTH_HEADER_<host>`, `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` for
an `AGENT` or `HARNESS` source; any `RUN --mount=type=secret,id=<ID>`), `<NAME>` one of yours. A label
under `kiln.secret.` that isn't such a reference isn't kept (422, rule `SecretReference`), its words
never repeating what it held. As an agent of yours starts from that Agentfile, kiln reads each named
secret from yours; one you hold none of refuses the start before anything is placed (400,
`… kiln.secret.<ID> names user:<NAME>, which isn't one of your secrets`), whoever's Agentfile has the
same text. Its runner gives them to that build alone (`shards build --secret id=<ID>,env=…`, the value in
the build's environment, never on its command line, in a layer or in the session it starts). The build
is given the Agentfile without its `kiln.secret.` labels (each line kept, so Shards' words name your
lines), so neither a reference nor a secret's name reaches the image: its config's labels, its build's
definition and source map, its provenance.

## WebSockets

* `GET /api/events` — server → client JSON messages:
  `{type:"run", run: Run}`, `{type:"job", job: Job}`, `{type:"agent", agent: Agent}`,
  `{type:"sample", agent: string, sample: HostSample}`, `{type:"queue", queued, running}`,
  `{type:"join_requests"}` (a join request arrived, was confirmed or decided: refetch
  `/api/join-requests`), `{type:"discovery"}` (an instance appeared, changed or left: refetch
  `/api/discovery`), `{type:"front_door", runner}` (a runner's front-door settings or status
  changed: refetch `/api/runners/:name/front-door`), `{type:"agentfile", owner, id, name, version,
  deleted?}` (one of a person's Agentfiles was kept, changed or deleted: to its owner, and whoever
  reaches everyone's; refetch `/api/agentfiles`), `{type:"ship", repo, rows: [{ row, yours?,
  whose?, dv?, base?, parts?, items? }], gone?, pace?, whole?}` (ci.md W21-R15: a repository's Ship
  rows that changed in one refresh of Ship's read model, each once, as `GET /api/ship/commits`
  renders them, for everyone and for those whose turn it is; `dv` once someone read that commit's
  detail, and where its detail moved from `base`, the parts that changed (`parts`, each whole) and
  the lists that changed entry by entry (`items`: `{ len, at: [{ i, v }] }`); `gone`, commits no
  longer held; `pace`, its pace line moved; `whole`, the repository went or was renamed. Sent after
  `run`, `job`, `review`, `comment`, `conflict`, `cd_apps`, `cd_rollout`, `updates` or `update_job`,
  at most every 250 ms, while a page listens and once Ship was read. Never coalesced: each says
  only what its refresh changed).
* `GET /api/live` — the browser protocol over one WebSocket per browser (docs/browser-protocol.md,
  "The WebSocket carrier"), served when `[server] live` is set (else 404): binary messages of goal
  2's frames both ways, opened with a single-use ticket from `POST /api/live/ticket`. The upgrade
  needs the session's credentials (401), an `Origin` of the server's own (403) and a
  `Sec-WebSocket-Protocol` naming a version it speaks (`kiln-bp.<n>`; else 400 naming them); the 101
  names the version and no extension. `GET /api/live/sockets`: the caller's own protocol sockets
  and their numbers.
* `GET /api/jobs/:id/logs/ws?step=N` — server sends the backlog then live output as **binary**
  frames (raw bytes, ANSI). Text frames: `{type:"step", idx, status}`, `{type:"end"}`.
* `GET /api/terminal/ws?target=local|agent:<name>&cols=120&rows=40` — full PTY.
  Client → server: **binary** = keystrokes, text `{type:"resize", cols, rows}`.
  Server → client: **binary** = PTY output, text `{type:"exit", code}` / `{type:"error", message}`.
  `?session=<id>` attaches to a running session: `{type:"session", session}`, its replay, then its
  output. With `&first=screen` the part of the replay that draws the session's screen comes ahead of
  it, where it has one (`session` then says `screen`: docs/wire.md, "A terminal's screen first").
* `GET /api/agents/tunnel` — **open**; an agent's QUIC datagrams when UDP to the QUIC port gets no
  answer (see architecture.md, *Agent transport*). Each **binary** message is one UDP datagram of at
  most 1500 bytes, both ways: the server relays it to its own QUIC listener from a loopback socket
  of this tunnel's, and the listener's datagrams back. Client → server text `bind <64 hex digits>`:
  the TLS exporter value (label `EXPORTER-kiln-transport-binding`, empty context, 32 bytes) of the
  QUIC connection the tunnel carries, which makes that agent's `transport` `"tunnel"`. 400 without a
  WebSocket upgrade, 429 past 60 new tunnels a minute from one address, 503 with 256 open. Closed
  after 10 minutes without a datagram, on either side's close, and on a message over the bound.
