# kiln HTTP / WebSocket API (browser + CLI)

Agents do **not** use this API except for `POST /api/agents/enroll`; they speak QUIC (see
`docs/protocol.md`). All JSON. Timestamps are unix seconds unless the field ends in `_ms`.

## Auth

Every `/api/*` route except `/api/auth/*`, `/api/agents/enroll`, `/api/hooks/*` requires either the
`kiln_session` cookie or `Authorization: Bearer <admin token>`. 401 → UI shows the login screen.

| Method | Path | Body / query | Result |
| --- | --- | --- | --- |
| POST | `/api/auth/login` | `{ "token": "…" }` | 204 + cookie |
| GET | `/api/auth/otp?code=…` | one-time code from `kiln login` | 302 → `/` + cookie |
| POST | `/api/auth/logout` | | 204 |
| GET | `/api/me` | | `{ "authenticated": true, "version": "0.1.0", "hostname": "…" }` |

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
  draining: boolean; sample: HostSample|null }
interface Child { name; state: "starting"|"running"|"restarting"|"stopped"|"finished"; restarts;
  last_error: string|null; since_unix }
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
| GET | `/api/runs?repo=&limit=50&before=` | `Run[]` newest first |
| GET | `/api/runs/:id` | `{ run: Run, jobs: Job[] }` |
| POST | `/api/runs/:id/cancel` | |
| POST | `/api/runs/:id/retry` | `{ only_failed: bool }` |
| GET | `/api/jobs/:id` | `Job` with `spec` |
| GET | `/api/jobs/:id/logs?step=N` | raw bytes (ANSI) of that step |
| GET | `/api/agents` | `Agent[]` |
| DELETE | `/api/agents/:name` | forget agent |
| POST | `/api/agents/:name/drain` | `{ draining: bool }` |
| GET | `/api/agents/join` | `{ token, server, command }` – bare-metal join command |
| GET | `/api/metrics/history?agent=` | `{ [agent]: HostSample[] }` last hour, 5 s resolution (`agent` optional; server is `"server"`) |
| GET | `/api/system` | `{ version, uptime_s, generation, children: Child[], crash_log: string[], config: object }` |
| GET | `/api/audit?limit=` | `{ts, actor, action, detail}[]` |
| POST | `/api/hooks/github` | GitHub webhook (HMAC `X-Hub-Signature-256`) → immediate poll |
| GET | `/metrics` | Prometheus exposition |
| GET | `/healthz`, `/readyz` | liveness / readiness |

## WebSockets

* `GET /api/events` — server → client JSON messages:
  `{type:"run", run: Run}`, `{type:"job", job: Job}`, `{type:"agent", agent: Agent}`,
  `{type:"sample", agent: string, sample: HostSample}`, `{type:"queue", queued, running}`.
* `GET /api/jobs/:id/logs/ws?step=N` — server sends the backlog then live output as **binary**
  frames (raw bytes, ANSI). Text frames: `{type:"step", idx, status}`, `{type:"end"}`.
* `GET /api/terminal/ws?target=local|agent:<name>&cols=120&rows=40` — full PTY.
  Client → server: **binary** = keystrokes, text `{type:"resize", cols, rows}`.
  Server → client: **binary** = PTY output, text `{type:"exit", code}` / `{type:"error", message}`.
