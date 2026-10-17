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
  /** Linux: `curl … /install.sh | sh -s -- …`. */
  command: string;
  /** macOS: the same script, with macOS labels. */
  macos?: string;
  /** Windows: the PowerShell installer (`install.ps1`). */
  windows?: string;
  /** Token-free install that asks to join (an admin approves it with a verification code). */
  join?: string;
  join_manual?: string;
}

export type JoinRequestStatus = "pending" | "approved" | "denied";

/** A machine asking to join as an agent (`kiln join`), GET /api/join-requests. */
export interface JoinRequest {
  id: string;
  name: string;
  hostname: string;
  os: string;
  arch: string;
  labels: string[];
  version: string;
  /** Where the request came from. */
  address: string;
  status: JoinRequestStatus;
  /** Six digits; null until the machine has confirmed the request. */
  code: string | null;
  created_at: number;
  expires_at: number;
  /** The agent name the approval gave. */
  agent: string | null;
}

/** A kiln instance heard on the local network (multicast DNS). */
export interface DiscoveredInstance {
  id: string;
  instance: string;
  /** Host name. */
  name: string;
  role: "server" | "agent" | string;
  version: string;
  os: string;
  arch: string;
  port: number;
  quic: number;
  ca: string | null;
  enrolled: string | null;
  addresses: string[];
  first_seen: number;
  last_seen: number;
  url: string | null;
  /** Already one of this server's agents (or runs one). */
  joined: boolean;
  agent: string | null;
  same_ca: boolean;
  join_command: string;
}

export interface DiscoveryInfo {
  enabled: boolean;
  announce: boolean;
  browse: boolean;
  join_requests: boolean;
  self: { id: string; name: string; instance: string; ca: string | null; port: number; quic: number; version: string };
  instances: DiscoveredInstance[];
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
  | { type: "queue"; queued: number; running: number }
  | { type: "terminals" }
  /** Join requests changed: refetch GET /api/join-requests. */
  | { type: "join_requests" }
  /** The discovery table changed: refetch GET /api/discovery. */
  | { type: "discovery" }
  /** A machine's AI sessions changed: refetch GET /api/ai-sessions. */
  | { type: "ai_sessions"; machine: string };

export type LogControlFrame = { type: "step"; idx: number; status: Status } | { type: "end" };

/** A live AI coding session (Claude Code, Codex, Gemini CLI) on a machine. */
export interface AiSession {
  /** The runner it was found on. */
  machine: string;
  tool: "claude" | "codex" | "gemini" | string;
  /** The tool's own session id ("" when it has none). */
  id: string;
  pid: number;
  /** The session's name, where the tool gives it one. */
  name: string;
  title: string;
  cwd: string;
  branch: string;
  model: string;
  started_at: number;
  last_activity: number;
  /** "working" (mid-turn), "waiting" (its turn ended), or "" (unknown). */
  state: string;
  last_prompt: string;
  last_reply: string;
  /** The kiln terminal session it runs in: attach to drive it. Null when started outside kiln. */
  terminal: string | null;
  /** Its transcript can be opened. */
  transcript: boolean;
}

/** One message of a transcript. */
export interface AiTranscriptEntry {
  role: "user" | "assistant" | "tool" | string;
  text: string;
  /** RFC 3339, or "". */
  at: string;
}

/** A terminal session kept by the server (any device can attach to it). */
export interface TerminalSessionInfo {
  id: string;
  /** "local" (the server) or "agent:<machine>" */
  target: string;
  command: string | null;
  /** Window title the program set (OSC 0/2). */
  title: string;
  created_at: number;
  attached: number;
  cols: number;
  rows: number;
  idle_seconds: number;
  exited: boolean;
  exit_code: number | null;
  error: string | null;
}

export type TerminalControlFrame =
  | { type: "exit"; code: number | null }
  | { type: "error"; message: string }
  /** First frame on every attach: which session this socket is on. */
  | { type: "session"; session: TerminalSessionInfo }
  /** The session isn't known (yet): e.g. its machine is reconnecting. Retry by id. */
  | { type: "missing"; session: string }
  /** The session's machine dropped; it keeps running there. Reattach by id. */
  | { type: "reconnect" };
