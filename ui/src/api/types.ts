import type { AgentInfo } from "./harness";
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
  /** Its runners' front doors (`https://kiln.home`, …). */
  origins?: string[];
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
  /** While a new runner replaces an old one without a restart: both processes, newest first. */
  instances?: RunnerInstance[];
}

/** One runner process of a machine (see `crates/kiln/src/instances.rs`). */
export interface RunnerInstance {
  /** "legacy" for a runner from before instances. */
  id: string;
  legacy: boolean;
  version: string;
  created_at: number;
  connected_at: number;
  pid: number | null;
  /** New work goes here. */
  newest: boolean;
  /** What it still holds (for an older one: what keeps it running). */
  terminals: number;
  agents: number;
  jobs: number;
  /** The build it runs (start of its binary's SHA-256); null for runners too old to say. */
  build?: string | null;
  /** It takes upgrade commands; older runners are upgraded through a terminal instead. */
  can_upgrade?: boolean;
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
  /** Windows: `irm <server>/install.ps1 | iex` (no token: asks to join with a verification code). */
  join_windows?: string;
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
  | { type: "ai_sessions"; machine: string }
  /** An agent session kiln runs appeared or changed. */
  | { type: "harness"; info: AgentInfo }
  /** An agent session kiln ran is gone. */
  | { type: "harness_closed"; id: string }
  /** Settings → Updates changed: refetch GET /api/updates. */
  | { type: "updates" }
  /** An upgrade moved on (new output, or it finished). */
  | { type: "update_job"; job: UpdateJob }
  /** A runner's front door changed (settings, or what the runner reported): refetch it. */
  | { type: "front_door"; runner: string }
  /** The tailnet's DNS for the front doors changed (credential, sync): refetch GET /api/tailscale. */
  | { type: "tailscale" }
  /** Apps' statuses changed (batched, at most every 300 ms). */
  | { type: "cd_apps"; apps: CdStatus[] }
  /** An app was deleted. */
  | { type: "cd_app_deleted"; app: string };

/* ---------------------------- front doors (Runners → Domain & HTTPS) ---------------------------- */

export type DnsMode = "resolver" | "authoritative";
/** `kiln`: self-signed, from kiln's own root (made, installed and renewed by kiln). `custom`: your own. */
export type CertMode = "kiln" | "custom";

/** A runner's front door, as set in the UI (`PUT /api/runners/:name/front-door`). */
export interface FrontDoorSettings {
  domain: string;
  aliases: string[];
  dns: { enabled: boolean; port: number; mode: DnsMode; upstream: string[]; addresses: string[] };
  /** `http_port` 0: no plain-HTTP redirect. `upstream` "": the runner's own server URL. */
  https: { enabled: boolean; port: number; http_port: number; upstream: string };
  cert: { mode: CertMode };
  mdns: { enabled: boolean; name: string };
  /** The front door on the tailnet (Tailscale). Absent while off. */
  tailscale?: { enabled: boolean };
}

export type FrontDoorState = "off" | "on" | "waiting" | "error";

export interface FrontDoorListener {
  state: FrontDoorState;
  /** `192.168.1.77:53`, `[::]:443`, `kiln.local`. */
  on: string[];
  error: string | null;
  /** HTTPS: where it forwards; DNS: its upstream resolvers. */
  to: string[];
  served: number;
  /** The one-time setup that fixes its error (also among the status's `fixes`). */
  fix: FrontDoorFix | null;
}

export interface CertInfo {
  subject: string;
  issuer: string;
  names: string[];
  not_before: number;
  not_after: number;
  sha256: string;
  server_auth: boolean;
}

export interface FrontDoorFix {
  why: string;
  commands: string[];
}

/** What the runner reports. */
export interface FrontDoorStatus {
  /** Fingerprint of the settings it applied. */
  settings: string;
  standby: boolean;
  os: string;
  /** The runner's own server URL (HTTPS forwards there by default). */
  server: string;
  addresses: string[];
  dns: FrontDoorListener;
  https: FrontDoorListener;
  http: FrontDoorListener;
  mdns: FrontDoorListener;
  /** `renewing`: a new self-signed certificate was asked for (renew now) and is not in yet. */
  cert: { mode: CertMode; state: FrontDoorState; info: CertInfo | null; kiln: boolean; missing: string[]; error: string | null; renewing: boolean };
  want: string[];
  asked: number;
  fixes: FrontDoorFix[];
  /** The front door on the tailnet, while `tailscale.enabled`. */
  tailscale?: TailnetStatus | null;
}

/** What the tailnet's split DNS says for one of the front door's names, as the runner sees it. */
export interface TailnetRoute {
  name: string;
  /** The route deciding it (the name, or a domain above it); null: none. */
  domain: string | null;
  to: string[];
  exit_node: boolean;
  /** It goes to this runner. */
  here: boolean;
}

/** The front door on the tailnet (`status.tailscale`). */
export interface TailnetStatus {
  state: FrontDoorState;
  error: string | null;
  fix: FrontDoorFix | null;
  /** The runner's MagicDNS name. */
  name: string;
  tailnet: string;
  addresses: string[];
  /** Where the tailnet's DNS for the names should go (its DNS answers there on port 53). */
  nameserver: string | null;
  magic_dns: boolean;
  accept_dns: boolean | null;
  routes: TailnetRoute[];
  /** The domain, asked of Tailscale's resolver (100.100.100.100) from the runner. */
  resolved: { name: string; answers: string[]; error: string | null } | null;
  health: string[];
  version: string;
}

/** `GET /api/tailscale`: the tailnet's split DNS for the front doors. */
export interface TailscaleView {
  configured: boolean;
  kind: "oauth" | "token" | null;
  /** The OAuth client's ID, or the token's first characters. */
  id: string | null;
  /** "configuration": routes are used with exit nodes too; "split-dns": they can't be. */
  mode: "configuration" | "split-dns" | null;
  exit_nodes: boolean | null;
  /** Name → the addresses the tailnet should send it to. */
  want: Record<string, string[]>;
  /** Names whose runner has not said where it listens yet: left as they are. */
  pending: string[];
  owned: Record<string, string[]>;
  synced_at: number | null;
  ok: boolean;
  error: string | null;
  /** The tailnet's routes for kiln's names, as of `synced_at`. */
  routes: Record<string, { addr: string; exit_node: boolean }[]>;
  replaced: Record<string, string[]>;
}

export interface FrontDoorView {
  runner: string;
  connected: boolean;
  /** null while the runner is offline (unknown). */
  supported: boolean | null;
  settings: FrontDoorSettings;
  /** The runner applied these settings; null before it reported. */
  applied: boolean | null;
  status: FrontDoorStatus | null;
  status_at: number | null;
  origins: string[];
  ca: { subject: string; sha256: string; not_after: number } | null;
}

/* ---------------------------- updates (Settings → Updates) ---------------------------- */

export interface UpdateJob {
  id: string;
  kind: "runner" | "server";
  /** The machine, or "server". */
  target: string;
  /** native: the runner's own upgrade command · terminal: through a terminal on an older runner ·
   * restart: Kubernetes rollout restart · replace: the server's binary replaced. */
  method: "native" | "terminal" | "restart" | "replace";
  state: "running" | "done" | "failed";
  started_at: number;
  ended_at: number | null;
  from_build: string | null;
  to_build: string | null;
  /** The machine reported running to_build. */
  reached: boolean;
  log: string[];
  error: string | null;
}

/** A kiln binary the server can hand out. */
export interface KilnBinary {
  target: string;
  sha256: string;
  size: number;
  build: string;
  /** server: the server's own · upload: uploaded here · release: fetched from GitHub · built: by kiln's own release pipeline. */
  origin: "server" | "upload" | "release" | "built";
  at: number;
  /** The release pipeline's run that built it, and the commit. */
  run?: number;
  sha?: string;
}

/** The pipeline that builds kiln for every platform, and its latest run. */
export interface ReleaseBuild {
  repo: string;
  pipeline: string;
  branch?: string;
  last?: Run | null;
  error?: string;
}

export interface ReleaseInfo {
  latest: string | null;
  newer: boolean;
  url: string | null;
  error: string | null;
  /** 0 until GitHub was asked. */
  checked_at: number;
}

/** Which build the server's image holds on its node (Kubernetes). */
export interface ImageCheck {
  image: string;
  node: string | null;
  build: string | null;
  newer: boolean;
  error: string | null;
  checked_at: number;
}

export interface ServerUpdate {
  version: string;
  build: string;
  target: string;
  mode: "kubernetes" | "service" | "standalone";
  deployment: string | null;
  namespace: string | null;
  image: ImageCheck | null;
  checking: boolean;
  newer: boolean;
  action: "restart" | "replace" | null;
  replacement: { origin: "upload" | "release"; build: string | null; version: string | null } | null;
  release: ReleaseInfo;
  note: string;
}

export interface RunnerUpdate {
  name: string;
  hostname: string;
  os: string;
  arch: string;
  connected: boolean;
  version: string;
  target: string | null;
  /** The build it would be upgraded to; null when kiln has no binary for its platform yet. */
  target_build: string | null;
  build: string | null;
  up_to_date: boolean;
  method: "native" | "terminal";
  terminal: boolean;
  instances: RunnerInstance[];
}

export interface UpdatesOverview {
  server: ServerUpdate;
  runners: RunnerUpdate[];
  jobs: UpdateJob[];
  binaries: KilnBinary[];
  targets: string[];
  build?: ReleaseBuild;
}

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
  /** Whether kiln can take it over as an agent: ready (it runs in a kiln terminal), stop (it runs outside kiln: taking over stops it there first), outside (an older server: quit it there first), no_id (still starting), upgrade (its machine's runner is too old to do it safely), unsupported. Older servers omit it. */
  takeover?: "ready" | "stop" | "outside" | "no_id" | "unsupported" | "upgrade";
  /** The kiln agent that runs this session, when kiln started it itself. */
  managed_by?: string;
  /** A copy of its conversation can be continued as an agent, leaving it running. Older servers omit it. */
  fork?: boolean;
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
  | { type: "reconnect" }
  /** The session's size changed (the device in use set it): draw at exactly this size. */
  | { type: "size"; cols: number; rows: number };

/* ---------------------------- deployments (docs/cd.md) ---------------------------- */

export type CdSync = "Synced" | "OutOfSync" | "Unknown";
export type CdHealth = "Healthy" | "Progressing" | "Degraded" | "Suspended" | "Missing" | "Unknown";

/** An app's sync: the one under way, or the last. */
export interface CdOperation {
  id: number;
  phase: "DryRun" | "Running" | "Failing" | "Terminating" | "Succeeded" | "Failed";
  trigger: "manual" | "rollback" | "promotion" | "auto" | "self-heal";
  attempt: number;
  message: string;
  /** Who started it, when a person or agent did. */
  by?: string | null;
  /** ms since the epoch. */
  started_ms?: number;
  /** ms since the epoch, once it finished. */
  finished_ms?: number | null;
}

/** A commit as people read it. */
export interface CdCommit {
  sha: string;
  author: string;
  email: string;
  /** ms since the epoch. */
  at_ms: number;
  subject: string;
  body: string;
}

/** An app's status, as `cd_apps` events carry it. */
export interface CdStatus {
  /** `<project>/<name>`. */
  app: string;
  sync: CdSync;
  health: CdHealth;
  /** The commit its sources are at. */
  revision?: string | null;
  /** Objects in git. */
  resources?: number;
  /** Objects that differ, missing and to-prune ones included. */
  out_of_sync?: number;
  /** Why the status is what it is, when that isn't obvious. */
  message?: string | null;
  operation?: CdOperation | null;
}

/** An app as lists show it. */
export interface CdApp extends CdStatus {
  name: string;
  project: string;
  cluster: string;
  namespace: string;
  environment: string | null;
  /** Syncs when git moves. */
  auto: boolean;
  labels: Record<string, string>;
  origin: { kind: "deploy"; repo: string } | { kind: "argo"; parent: string | null } | { kind: "api" };
  /** Adopted from Argo CD and not taken over: kiln compares it and never writes. */
  shadow: boolean;
  /** Held at an earlier deployment by a rollback, until git changes the app. */
  pinned?: CdPin | null;
  /** Why its sources can't be read. */
  source_error?: string;
}

/** A rollback's hold on an app. */
export interface CdPin {
  /** The deployment it went back to. */
  to: number;
  revision: string | null;
  by: string | null;
  at_ms: number;
}

/** One of an app's sources. */
export interface CdSource {
  repo: string;
  path: string;
  revision: string;
  chart: string | null;
  as_ref: string | null;
}

export interface CdAppDetail extends CdApp {
  definition: {
    sources: CdSource[];
    sync: { auto: boolean; prune: boolean; self_heal: boolean; allow_empty: boolean };
    [k: string]: unknown;
  } | null;
  /** Each source's commit, as last resolved. */
  commits: (string | null)[];
  /** Apps it renders (app of apps), by id. */
  children: string[];
  /** Who wrote the revision it is at, when, and what it says. */
  revision_info: CdCommit | null;
}

/** A label and value people read on a node (`Ready` · `2/2`). */
export interface CdInfo {
  name: string;
  value: string;
}

/** One container of a pod, or of a workload's pod template. */
export interface CdContainer {
  name: string;
  image: string;
  init: boolean;
  ready: boolean | null;
  restarts: number | null;
  state: "running" | "waiting" | "terminated" | null;
  reason: string | null;
  /** Why its previous run ended, when it restarted. */
  last: string | null;
}

/** One node of an app's resource tree: an object the app manages, or a live object they own. */
export interface CdNode extends CdResourceKey {
  /** Unique in the tree: the uid, or the key of an object not in the cluster. */
  id: string;
  uid: string | null;
  version: string;
  /** Owners' uids, among the tree's nodes. */
  parents: string[];
  managed: boolean;
  health: CdHealth | null;
  message: string | null;
  sync: CdSync | "Hook" | null;
  hook: boolean;
  missing: boolean;
  requires_pruning: boolean;
  why: string | null;
  created_at: string | null;
  info: CdInfo[];
  /** A pod's state in kubectl's words. */
  status: string | null;
  ready: string | null;
  restarts: number | null;
  images: string[];
  containers: CdContainer[];
  /** The node a pod runs on. */
  node: string | null;
}

export interface CdTree {
  app: string;
  cluster: string;
  namespace: string;
  nodes: CdNode[];
  truncated: boolean;
  /** What couldn't be read. */
  errors: string[];
  at_ms: number;
}

export interface CdCondition {
  type: string;
  status: string;
  reason: string;
  message: string;
  last: string | null;
}

/** One object of an app's tree in depth. */
export interface CdResourceDetail {
  key: CdResourceKey;
  managed: boolean;
  node: CdNode | null;
  /** The live object as YAML, null when it isn't in the cluster. */
  live: string | null;
  /** What kiln applies, for managed objects. */
  desired: string | null;
  diff: CdDiff | null;
  labels: Record<string, string>;
  annotations: Record<string, string>;
  conditions: CdCondition[];
  finalizers: string[];
  /** Nearest first. */
  owners: (CdResourceKey & { id: string; uid: string | null })[];
  errors: string[];
}

export interface CdEvent {
  type: "Normal" | "Warning" | string;
  reason: string;
  message: string;
  count: number;
  first: string;
  last: string;
  source: string;
  object: { kind: string | null; name: string | null };
}

export interface CdLogLine {
  pod: string;
  /** RFC 3339, or "" when the line had none. */
  ts: string;
  text: string;
}

export interface CdLogs {
  pods: string[];
  /** Pods past the few read. */
  more_pods: number;
  container: string | null;
  containers: string[];
  lines: CdLogLine[];
  errors: string[];
}

export interface CdResourceKey {
  group: string;
  kind: string;
  namespace: string;
  name: string;
}

export interface CdResource extends CdResourceKey {
  /** "Hook" for hooks, which run rather than compare. */
  sync: CdSync | "Hook";
  missing: boolean;
  requires_pruning: boolean;
  health: CdHealth | null;
  message: string | null;
  hook: boolean;
  /** Where it differs: `.spec.replicas differs`. */
  why: string | null;
}

/** A unified-diff hunk; each line keeps its ' ', '-' or '+'. */
export interface CdHunk {
  old_start: number;
  old_lines: number;
  new_start: number;
  new_lines: number;
  lines: string[];
}

/** What a sync changes in one object. */
export interface CdDiff {
  key: CdResourceKey;
  why: string | null;
  /** The live object as YAML ("" when it is missing). */
  live: string;
  /** What a sync makes of it ("" when it is pruned). */
  predicted: string;
  /** From live to predicted. */
  hunks: CdHunk[];
}

/** One deployment of an app. */
export interface CdDeployment {
  app: string;
  /** Its number in the app's history. */
  id: number;
  /** Each source's commit. */
  commits?: (string | null)[];
  op: number;
  revision: string | null;
  trigger: string;
  by: string | null;
  succeeded: boolean;
  message: string;
  applied: number;
  pruned: number;
  /** ms since the epoch. */
  at_ms: number;
  /** What the deployed commit says. */
  commit?: CdCommit;
}

export interface CdCluster {
  name: string;
  server: string;
  labels: Record<string, string>;
  mode: string;
  auth: string;
  connected: boolean;
  version: string | null;
  kinds: number | null;
}

export interface CdSummary {
  apps: number;
  sync: Record<string, number>;
  health: Record<string, number>;
  /** Connected clusters. */
  clusters: number;
}

/** How many apps each filter value would show given the other filters, and how many match all. */
export interface CdFacets {
  total: number;
  facets: {
    sync: Record<string, number>;
    health: Record<string, number>;
    cluster: Record<string, number>;
    project: Record<string, number>;
    namespace: Record<string, number>;
  };
}

/* ---------------------------- discovery v2 + Add machines ---------------------------- */
// The fields and types of .claude/contracts/discovery.md. The interfaces named like ones above are
// merged into them (declaration merging), so this block adds to them without touching them.
// Every field is optional: servers from before discovery v2 leave them out.

/** How an instance was heard: multicast DNS, the OS's own DNS-SD (Bonjour on macOS, DNS-SD on
 * Windows), unicast DNS-SD, a network scan, a probe of one address, Tailscale, a seed address. */
export type DiscoveryVia = "mdns" | "native" | "dns-sd" | "scan" | "probe" | "tailscale" | "seed";

export interface DiscoveredInstance {
  via?: DiscoveryVia[];
  /** This server invited it (POST /api/discovery/invite). */
  invited?: boolean;
  /** The id of its pending join request, if any. */
  request?: string | null;
}

export interface DiscoveryInfo {
  methods?: DiscoveryMethod[];
  problems?: DiscoveryProblem[];
  /** Detected LAN IPv4 subnets, e.g. "192.168.1.0/24": what a scan covers by default. */
  subnets?: string[];
  /** The latest on-demand scan. */
  scan?: ScanState | null;
  dns_sd?: DnsSdInfo;
}

/** One way of finding kiln machines, and how it is doing. */
export interface DiscoveryMethod {
  id: "mdns" | "native" | "dns-sd" | "scan" | "tailscale" | "seeds";
  /** "Multicast DNS", "Bonjour (macOS)", "Windows DNS-SD", "DNS-SD (example.com)", "Network scan", "Tailscale", "Seed addresses". */
  label: string;
  state: "active" | "off" | "unavailable" | "error";
  /** One line: interfaces, domain, why it is unavailable, the error. */
  detail: string;
  /** Instances currently known through it. */
  heard: number;
  last_heard: number | null;
}

/** Something keeping machines from finding each other, with what fixes it. */
export interface DiscoveryProblem {
  /** Stable, e.g. "mdns-port", "no-multicast-iface", "silent-network", "dns-sd-publish". */
  id: string;
  /** What is wrong, in one or two sentences. */
  text: string;
  fixes: DiscoveryFix[];
}

export interface DiscoveryFix {
  /** The system it is for; none: any. */
  os?: InstallOs;
  text?: string;
  command?: string;
}

export interface ScanState {
  running: boolean;
  cidrs: string[];
  started_at: number;
  finished_at: number | null;
  /** Addresses probed so far, of `total`. */
  probed: number;
  total: number;
  /** New instances this scan found. */
  found: number;
}

/** Unicast DNS-SD: the domains browsed and publishing this server into a zone. */
export interface DnsSdInfo {
  /** Config, search domains and discovered browse domains. */
  domains: string[];
  publish: { configured: boolean; zone: string | null; ok: boolean | null; error: string | null; at: number | null };
  /** Zone-file lines an admin can add by hand to publish this server. */
  records: string[];
}

export type InstallOs = "linux" | "macos" | "windows";

export interface JoinInfo {
  /** One-liners with an embedded join token: download kiln from this server, install it as a service, join. */
  install?: Record<InstallOs, string>;
  /** When that token stops working, if it expires. */
  expires_at?: number | null;
}

/* ---------------------------- folders on a machine (GET /api/machines/:target/dirs) ---------------------------- */

/** One folder inside another. */
export interface DirEntry {
  name: string;
  /** Last modified, unix seconds (0: unknown). */
  modified: number;
}

/** The folders in one folder on a machine, most recently modified first (at most 200). */
export interface DirListing {
  /** The folder, normalized: `~` form under the home of the user kiln runs as. */
  path: string;
  /** The same folder, absolute. */
  abs: string;
  /** The folder above it (`~` form under home); null at the root. */
  parent: string | null;
  /** A folder is there. False when nothing is (or not a folder); true when one is but can't be read. */
  exists: boolean;
  dirs: DirEntry[];
  /** It has more folders than were sent. */
  truncated: boolean;
  /** Why it can't be listed: "no such folder", "not a folder", "permission denied", ... */
  error?: string;
}

/* ------------------------------------------------------------ repo sync (docs/sync.md) */

export interface SyncSkipped {
  path: string;
  why: string;
}

/** How a copy is doing, as its machine last said. */
export interface SyncCopyStatus {
  applied: number;
  behind: number;
  unsent: number;
  waiting: number;
  conflicts: number;
  skipped: SyncSkipped[];
  error: string | null;
  at: number;
}

export type SyncCopyState = "in step" | "catching up" | "fetching" | "sending" | "starting" | "offline" | "error";

export interface SyncCopy {
  member: number;
  machine: string;
  path: string;
  added: number;
  state: SyncCopyState;
  status: SyncCopyStatus | null;
}

export interface SyncConflict {
  /** The conflict copy. */
  path: string;
  /** The path it stands beside. */
  beside: string;
}

export interface SyncRepo {
  id: string;
  name: string;
  created: number;
  exclude: string[];
  files: number;
  index: number;
  conflicts: SyncConflict[];
  copies: SyncCopy[];
  error: string | null;
}

export interface SyncView {
  repos: SyncRepo[];
  error: string | null;
}

export type SyncKeep = "name" | "copy" | "both";
