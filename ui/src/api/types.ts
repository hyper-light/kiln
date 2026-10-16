// Types mirror docs/api.md (and crates/kiln/src/store.rs). Timestamps are unix
// seconds unless the field name ends in `_ms`. Ids are opaque strings.

export type Status =
  | "pending"
  | "queued"
  | "running"
  | "succeeded"
  | "failed"
  | "cancelled"
  | "skipped"
  | "timed_out";

export const STATUSES: Status[] = [
  "pending",
  "queued",
  "running",
  "succeeded",
  "failed",
  "cancelled",
  "skipped",
  "timed_out",
];

export type RunEvent = "push" | "pull_request" | "tag" | "manual" | "schedule";

export interface Me {
  authenticated: boolean;
  version: string;
  hostname: string;
  /** Where the server says it is reachable from other devices. */
  public_url?: string;
}

export interface Repo {
  id: string;
  /** owner/repo */
  name: string;
  clone_url: string;
  branches: string[];
  poll_seconds: number;
  enabled: boolean;
  created_at: number;
  last_polled_at: number | null;
  last_error: string | null;
}

export interface Run {
  id: string;
  repo_id: string;
  repo_name: string;
  number: number;
  pipeline: string;
  event: RunEvent | string;
  branch: string;
  sha: string;
  message: string;
  author: string;
  status: Status;
  created_at: number;
  started_at: number | null;
  finished_at: number | null;
  error: string | null;
  concurrency_group: string | null;
}

export interface Step {
  idx: number;
  name: string;
  status: Status;
  started_at: number | null;
  finished_at: number | null;
  exit_code: number | null;
}

export interface StepSpec {
  name?: string | null;
  run: string;
  shell?: string | null;
  env?: Record<string, string>;
  "working-directory"?: string | null;
  "timeout-minutes"?: number | null;
  "continue-on-error"?: boolean;
  tty?: boolean;
}

export interface PlannedJob {
  key: string;
  spec_id: string;
  display: string;
  needs: string[];
  labels: string[];
  image: string | null;
  toolchain: string | null;
  env: Record<string, string>;
  timeout_minutes: number;
  retries: number;
  exclusive: boolean;
  resources: { cpu: string | null; memory: string | null };
  artifacts: string[];
  cache: string[];
  condition: string | null;
  steps: StepSpec[];
}

export type Executor = "baremetal" | "k8s";

export interface Job {
  id: string;
  run_id: string;
  key: string;
  display: string;
  status: Status;
  labels: string[];
  /** job keys */
  needs: string[];
  agent: string | null;
  executor: Executor | null;
  attempt: number;
  queued_at: number | null;
  started_at: number | null;
  finished_at: number | null;
  exit_code: number | null;
  error: string | null;
  /** bytes */
  peak_rss: number | null;
  cpu_seconds: number | null;
  steps: Step[];
  spec?: PlannedJob;
}

export interface HostSample {
  ts_ms: number;
  cpu_pct: number;
  cpus: number;
  load1: number;
  mem_total: number;
  mem_used: number;
  disk_total: number;
  disk_used: number;
  /** cumulative byte counters */
  net_rx: number;
  net_tx: number;
  procs: number;
}

export interface Agent {
  name: string;
  labels: string[];
  slots: number;
  hostname: string;
  os: string;
  arch: string;
  version: string;
  ephemeral: boolean;
  connected: boolean;
  last_seen: number;
  enrolled_at: number;
  /** job ids */
  running: string[];
  busy: number;
  draining: boolean;
  sample: HostSample | null;
}

export type ChildState = "starting" | "running" | "restarting" | "stopped" | "finished";

export interface Child {
  name: string;
  state: ChildState;
  restarts: number;
  last_error: string | null;
  since_unix: number;
}

export interface HourlyBucket {
  ts: number;
  succeeded: number;
  failed: number;
  avg_seconds: number;
}

export interface OverviewStats {
  runs_24h: number;
  runs_failed_24h: number;
  runs_succeeded_24h: number;
  median_job_seconds: number;
  jobs_by_status: Partial<Record<Status, number>>;
  hourly: HourlyBucket[];
}

export interface Overview {
  stats: OverviewStats;
  queue: { queued: number; running: number };
  agents: { connected: number; total: number; slots: number; busy: number };
  system: {
    version: string;
    uptime_s: number;
    hostname: string;
    generation: number;
    children: Child[];
    sample: HostSample | null;
  };
}

export interface RunDetail {
  run: Run;
  jobs: Job[];
}

export interface JoinInfo {
  token: string;
  server: string;
  command: string;
}

export type MetricsHistory = Record<string, HostSample[]>;

export interface SystemInfo {
  version: string;
  uptime_s: number;
  generation: number;
  children: Child[];
  crash_log: string[];
  config: Record<string, unknown>;
}

export interface AuditEntry {
  ts: number;
  actor: string;
  action: string;
  detail: string;
}

export interface NewRepo {
  name: string;
  clone_url?: string;
  branches?: string[];
  poll_seconds?: number;
}

/* ---------------------------- WebSocket frames ---------------------------- */

export type ServerEvent =
  | { type: "run"; run: Run }
  | { type: "job"; job: Job }
  | { type: "agent"; agent: Agent }
  | { type: "sample"; agent: string; sample: HostSample }
  | { type: "queue"; queued: number; running: number };

export type LogControlFrame = { type: "step"; idx: number; status: Status } | { type: "end" };

export type TerminalControlFrame =
  | { type: "exit"; code: number | null }
  | { type: "error"; message: string };
