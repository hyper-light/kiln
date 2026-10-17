/**
 * kiln as the harness: the normalized agent-session model (mirrors crates/kiln/src/harness/model.rs),
 * the API to list, start and stop agents, and a live session store fed by its websocket.
 * Nothing here is specific to one harness: Claude Code, Codex and the rest all speak this.
 */
import * as React from "react";
import { wsUrl } from "@/api/client";
import type { TerminalSessionInfo } from "./types";

/* ------------------------------------------------------------------ model */

export type Level = "info" | "warn" | "error";
export type ToolKind = "shell" | "edit" | "read" | "search" | "web" | "agent" | "skill" | "todo" | "question" | "plan" | "mcp" | "other";
export type ToolStatus = "pending" | "running" | "done" | "error" | "denied" | "cancelled";
export type Status = "starting" | "idle" | "running" | "waiting" | "exited" | "disconnected";
export type SendMode = "queue" | "steer" | "now";
export type Delivery = "sent" | "queued" | "started" | "completed" | "cancelled";
export type Decision = "allow" | "always" | "deny";

export interface Hunk {
  old_start: number;
  old_lines: number;
  new_start: number;
  new_lines: number;
  /** Each line keeps its ' ', '-' or '+' prefix. */
  lines: string[];
}

export interface FileDiff {
  path: string;
  created?: boolean;
  hunks: Hunk[];
}

export interface Todo {
  text: string;
  status: "pending" | "in_progress" | "completed" | string;
}

export interface ToolCall {
  tool_id: string;
  name: string;
  tool: ToolKind;
  title: string;
  detail?: string;
  input: unknown;
  status: ToolStatus;
  output?: string;
  truncated?: boolean;
  diffs?: FileDiff[];
  todos?: Todo[];
  task?: string;
  exit_code?: number;
  started: number;
  ended?: number;
  /** MCP calls: the server and the tool's own name. */
  server?: string;
  tool_short?: string;
  /** Skill calls: which skill. */
  skill?: string;
}

export type Body =
  | { kind: "user"; text: string; images?: number; attachments?: string[]; client_id?: string; origin?: string }
  | { kind: "assistant"; text: string; streaming?: boolean }
  | { kind: "thinking"; text: string; streaming?: boolean; redacted?: boolean }
  | ({ kind: "tool" } & ToolCall)
  | { kind: "notice"; level: Level; text: string };

export type HEvent = { id: string; seq: number; at: number; parent?: string } & Body;

export interface AlwaysOption {
  label: string;
  scope: string;
  value: unknown;
}

export interface QuestionOption {
  label: string;
  description?: string;
  /** Shown while the option is focused or picked (a mockup, a snippet). */
  preview?: string;
}

export interface Question {
  /** The harness's own id for it (Codex). */
  id?: string;
  question: string;
  header?: string;
  multi?: boolean;
  options: QuestionOption[];
  /** An answer in your own words is allowed (Claude always; Codex says). */
  other?: boolean;
  /** Mask what's typed. */
  secret?: boolean;
}

export interface Approval {
  id: string;
  tool_id?: string | null;
  tool_name: string;
  tool: ToolKind;
  title: string;
  detail?: string;
  input: unknown;
  command?: string;
  cwd: string;
  diffs?: FileDiff[];
  reason?: string;
  always?: AlwaysOption[];
  default_to_no?: boolean;
  questions?: Question[];
  plan?: string;
  agent?: string;
  at: number;
}

export interface QueueItem {
  id: string;
  text: string;
  at: number;
}

export interface Task {
  id: string;
  tool_id?: string;
  description: string;
  kind?: string;
  status: string;
  background?: boolean;
  depth?: number;
  parent_task?: string;
  prompt?: string;
  summary?: string;
  last_tool?: string;
  tokens?: number;
  tool_uses?: number;
  duration_ms?: number;
  started: number;
  ended?: number;
  /** What it is: a shell or monitor the agent left running, a subagent, a workflow, a backgrounded MCP call. */
  class?: "shell" | "monitor" | "agent" | "workflow" | "mcp";
  /** What a shell or monitor runs, and where. */
  command?: string;
  cwd?: string;
  /** Where its output is written on the agent's machine (display only; read it with taskOutput). */
  output?: string;
  process?: string;
  exit_code?: number;
  /** Monitors: how many events it has seen, and the latest. */
  events?: number;
  last_event?: string;
  /** Monitors: whom an event wakes ("agent", or a subagent task's id). */
  wakes?: string;
  /** Whether a stop can work. */
  stoppable?: boolean;
}

/** A slice of a task's output (`since` → `offset`); `gone` once the harness deleted it. */
export interface TaskOutput {
  data?: string;
  offset?: number;
  live?: boolean;
  truncated?: boolean;
  path?: string;
  gone?: boolean;
  error?: string;
}

export interface Usage {
  cost_usd: number;
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number;
  turns: number;
  context_tokens?: number;
  context_window?: number;
}

/** An MCP server the agent can use. `target` is the command line or URL, never with secrets. */
export interface McpServer {
  name: string;
  scope: string;
  transport: string;
  status: "connected" | "pending" | "failed" | "needs_auth" | "disabled" | string;
  error?: string;
  enabled: boolean;
  tools: { name: string; description?: string }[];
  resources?: number;
  prompts?: number;
  target?: string;
}

export interface Skill {
  name: string;
  description: string;
  source: string;
  path?: string;
  args_hint?: string;
  user_invocable: boolean;
}

export interface Plugin {
  name: string;
  source?: string;
  enabled: boolean;
  skills?: string[];
  commands?: string[];
}

/** A plan limit of the account the agent runs as (5-hour window, weekly …). */
export interface Limit {
  id: string;
  label: string;
  used_percent: number | null;
  used?: number;
  limit?: number;
  unit?: string;
  /** Unix ms. */
  resets_at?: number;
  window?: string;
  /** A stable, non-secret id of the account ("claude:max:3f2a…"), the same across its agents. */
  account?: string;
  /** "allowed", "warning" (close to the limit) or "rejected" (reached), when the harness says. */
  status?: string;
}

/** One line of the harness's status: version, account, model, sandbox, MCP servers … */
export interface StatusItem {
  group: string;
  label: string;
  value: string;
  tone?: "ok" | "warn" | "error";
}

export interface ExitInfo {
  code: number | null;
  error?: string;
}

/** One file's change since the agent first touched it (the server keeps it cumulative). */
export interface FileChange {
  path: string;
  created?: boolean;
  deleted?: boolean;
  added: number;
  removed: number;
  hunks: Hunk[];
  truncated?: boolean;
  at: number;
}

export interface State {
  status: Status;
  goal?: string;
  turn_started?: number;
  summary?: string;
  /** What it is doing right now, in plain words. */
  doing?: string;
  files_changed?: number;
  lines_added?: number;
  lines_removed?: number;
  harness_session?: string;
  cwd: string;
  model: string;
  mode: string;
  title: string;
  approvals: Approval[];
  queue: QueueItem[];
  delivery: Record<string, Delivery>;
  tasks: Task[];
  todos: Todo[];
  usage: Usage;
  activity?: string;
  exit?: ExitInfo;
  dropped: number;
  effort?: string;
  /** Live values of Meta.controls, by id. */
  control_values?: Record<string, unknown>;
  limits?: Limit[];
  /** The harness's own status lines (`status` is the session's lifecycle). */
  harness_status?: StatusItem[];
  mcp?: McpServer[];
  skills?: Skill[];
  plugins?: Plugin[];
  /** The account's plan as the harness reports it ("Max", "Pro", "API key", …) and a stable account id. */
  plan?: string;
  account?: string;
  /** Shells, monitors and other tasks still running while the agent is idle or ended. */
  left_behind?: number;
  /** Which of those lists the harness has reported yet (servers that predate it omit this). */
  inventory?: { mcp?: Known; skills?: Known; plugins?: Known; limits?: Known };
}

/** loading: not reported yet; ready: reported (an empty list means none); unsupported: the harness can't say. */
export type Known = "loading" | "ready" | "unsupported";

export interface ModeOption {
  id: string;
  label: string;
  description: string;
  dangerous?: boolean;
}

export interface ModelOption {
  id: string;
  label: string;
  description?: string;
}

export interface SlashCommand {
  name: string;
  description?: string;
  hint?: string;
  /** "Session", "Model & mode", "Context", "Tools & MCP", "Skills", "Plugins", "Kiln". */
  group?: string;
  /** Whether it takes words after its name. */
  args?: "none" | "optional" | "required";
  aliases?: string[];
  /** What running it does (servers that predate the catalog send none: a prompt). */
  route?:
    | { kind: "prompt"; text?: string; skill?: boolean }
    | { kind: "control"; command: Record<string, unknown> }
    | { kind: "view"; view: string }
    | { kind: "unsupported"; reason: string }
    /** Done in the harness's own screen, opened beside the agent on `command`. */
    | { kind: "terminal"; command: string };
}

/** A terminal beside an agent, in its folder on its machine: its shell, or its harness's own screen. */
export interface BesideTerminal {
  terminal: TerminalSessionInfo;
  what: "shell" | "screen";
  /** The screen's slash command, when it was opened on one. */
  command?: string;
  /** That command is for you to type there (the harness can't start on one). */
  to_type?: boolean;
}

/** Which agents a broadcast picks: `ids` exactly; else `all`, or filters that must all match (none: nothing). */
export interface BroadcastTo {
  all?: boolean;
  ids?: string[];
  machines?: string[];
  kinds?: string[];
  status?: string[];
}

export interface BroadcastBody {
  to: BroadcastTo;
  text?: string;
  mode?: SendMode;
  command?: Command;
  approvals?: "allow" | "deny";
}

export interface BroadcastOutcome {
  id: string;
  name: string;
  machine: string;
  kind: string;
  result: "sent" | "skipped" | "failed";
  reason?: string;
}

export interface BroadcastReport {
  id: string;
  sent: number;
  skipped: number;
  failed: number;
  results: BroadcastOutcome[];
}

type Counted<T> = T & { count: number };

export interface BroadcastPreview {
  agents: { id: string; name: string; machine: string; kind: string; status: Status }[];
  machines: string[];
  commands: Counted<SlashCommand>[];
  modes: Counted<ModeOption>[];
  models: Counted<ModelOption>[];
  efforts: Counted<ControlOption>[];
}

export interface ControlOption {
  id: string;
  label: string;
  description?: string;
  dangerous?: boolean;
}

/**
 * Anything a harness lets you set or do, described generically so the console can offer it without
 * knowing the harness: effort, sandboxes, MCP servers, compaction, rewinds, output styles ...
 */
export interface Control {
  id: string;
  group: string;
  label: string;
  description?: string;
  kind: "select" | "toggle" | "action" | "text";
  options?: ControlOption[];
  value?: unknown;
  dangerous?: boolean;
  /** Can be set for one message only. */
  per_turn?: boolean;
  /** "turn": an action on one turn (rewind to it, branch from it). */
  scope?: "session" | "turn";
  requires?: string;
  /** For an action that takes free text (compaction instructions …): the field's placeholder. */
  input?: string;
}

/** A file sent with a message: inline, or uploaded first (`harnessApi.upload`) and referenced by id. */
export interface Attachment {
  name: string;
  mime: string;
  data_base64?: string;
  file_id?: string;
}

/** A hook the harness runs on an event (read-only). */
export interface HookInfo {
  event: string;
  matcher?: string;
  command: string;
  source?: string;
}

export interface Meta {
  version: string;
  controls?: Control[];
  hooks?: HookInfo[];
  /** Reasoning effort levels, when the harness has them. */
  efforts?: ControlOption[];
  modes: ModeOption[];
  models: ModelOption[];
  commands: SlashCommand[];
  agents: string[];
  /** "steer", "send_now", "queue_cancel", "interrupt", "set_mode", "set_model", "stop_task", "file_suggestions", "resume" */
  features: string[];
}

export interface ApprovalBrief {
  id: string;
  tool: ToolKind;
  tool_name: string;
  title: string;
  at: number;
}

/** An agent session as lists show it. */
export interface AgentInfo {
  id: string;
  kind: string;
  kind_name: string;
  /** "local" or "agent:<machine>". */
  target: string;
  name: string;
  shell?: string;
  cwd: string;
  created_at: number;
  status: Status;
  title: string;
  model: string;
  mode: string;
  harness_session?: string;
  approvals: ApprovalBrief[];
  queued: number;
  tasks_running: number;
  /** Shells, monitors and other tasks still running while the agent is idle or ended. */
  left_behind?: number;
  cost_usd: number;
  activity?: string;
  last_activity: number;
  exit?: ExitInfo;
  /** Board summary (optional: older servers don't send them). */
  goal?: string;
  summary?: string;
  todos_done?: number;
  todos_total?: number;
  files_changed?: number;
  lines_added?: number;
  lines_removed?: number;
  context_pct?: number;
  doing?: string;
  turn_started?: number;
  limits?: Limit[];
  account?: string;
  plan?: string;
}

export interface KindInfo {
  id: string;
  name: string;
  verified: boolean;
  available: boolean;
}

/** What to start. */
/** A conversation that ended on a machine, to continue with `spec.resume`. */
export interface PastSession {
  kind: string;
  id: string;
  title: string;
  cwd: string;
  branch?: string;
  model?: string;
  /** Unix seconds. */
  last_activity: number;
  last_prompt?: string;
}

export interface Spec {
  kind: string;
  cwd?: string;
  model?: string;
  mode?: string;
  resume?: string;
  prompt?: string;
  name?: string;
  shell?: string;
  allow_bypass?: boolean;
}

export type Command =
  | { type: "send"; id: string; text: string; mode?: SendMode; attachments?: Attachment[]; overrides?: { model?: string; effort?: string } }
  | { type: "control"; id: string; value?: unknown }
  | { type: "mcp_enable"; name: string; enabled: boolean }
  | { type: "mcp_reconnect"; name: string }
  | { type: "mcp_auth"; name: string; client: string }
  | { type: "mcp_add"; scope: string; name: string; transport: string; command?: string; args?: string[]; env?: Record<string, string>; url?: string; headers?: Record<string, string> }
  | { type: "mcp_remove"; scope: string; name: string }
  | { type: "skill_view"; name: string; client: string }
  | { type: "skill_run"; name: string; args?: string }
  | { type: "set_effort"; effort: string }
  | { type: "queue_edit"; id: string; text: string }
  | { type: "queue_remove"; id: string }
  | { type: "queue_move"; id: string; index: number }
  | { type: "queue_send_now"; id: string }
  | {
      type: "approve";
      id: string;
      decision: Decision;
      message?: string;
      always?: number;
      /** The older form: one string per question (runners from before `choices`). */
      answers?: Record<string, string>;
      /** Each question's answer as picked: labels, your own words, a note. Wins over `answers`. */
      choices?: { question: string; id?: string; selected: string[]; other?: string; note?: string }[];
    }
  | { type: "interrupt" }
  | { type: "set_mode"; mode: string }
  | { type: "set_model"; model: string }
  | { type: "stop_task"; task: string }
  | { type: "stop_tasks" }
  /** Take a queued message out of the queue and steer it into the running turn (no interrupt). */
  | { type: "queue_steer"; id: string }
  | { type: "restart" }
  | { type: "suggest"; client: string; query: string }
  | { type: "snapshot" };

export type Update =
  | { t: "open"; info: AgentInfo }
  | { t: "changes"; files: FileChange[] }
  | { t: "error"; error: string }
  | { t: "closed" }
  | { t: "state"; state: State }
  | { t: "meta"; meta: Meta }
  | { t: "events"; events: HEvent[] }
  | { t: "stream"; id: string; text: string }
  | { t: "reply"; client: string; value: unknown }
  | { t: "reset" };

/* ------------------------------------------------------------------ REST */

async function call<T>(method: string, path: string, body?: unknown): Promise<T> {
  const res = await fetch(path, {
    method,
    credentials: "same-origin",
    headers: body === undefined ? undefined : { "content-type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  if (!res.ok) {
    let msg = `${res.status} ${res.statusText}`;
    try {
      const j = (await res.json()) as { error?: string };
      if (j.error) msg = j.error;
    } catch {
      /* not json */
    }
    throw new Error(msg);
  }
  if (res.status === 204) return undefined as T;
  return (await res.json()) as T;
}

export const harnessApi = {
  kinds: (target: string) => call<KindInfo[]>("GET", `/api/harness/kinds?target=${encodeURIComponent(target)}`),
  /** Every connected machine's harnesses, by target ("local", "agent:<name>"). */
  kindsAll: () => call<Record<string, KindInfo[]>>("GET", "/api/harness/kinds"),
  list: () => call<AgentInfo[]>("GET", "/api/harness/sessions"),
  /** The terminals beside an agent (its shell, its harness's own screen), live ones first. */
  terminals: (id: string) => call<BesideTerminal[]>("GET", `/api/harness/sessions/${encodeURIComponent(id)}/terminals`),
  /** Open one beside it: its shell (the one already open, with `command` run in it), or its harness's own screen on `command`. */
  openTerminal: (id: string, body: { what: "shell" | "screen"; command?: string; cols?: number; rows?: number }) =>
    call<BesideTerminal>("POST", `/api/harness/sessions/${encodeURIComponent(id)}/terminals`, body),
  /** A machine's recent ended conversations (newest first), for one harness or all. */
  history: (target: string, kind?: string, limit = 40) =>
    call<PastSession[]>("GET", `/api/harness/history?target=${encodeURIComponent(target)}${kind ? `&kind=${encodeURIComponent(kind)}` : ""}&limit=${limit}`),
  /** A task's output: its tail, or what came after byte `since`. */
  taskOutput: (id: string, task: string, since?: number) =>
    call<TaskOutput>("GET", `/api/harness/sessions/${encodeURIComponent(id)}/tasks/${encodeURIComponent(task)}/output${since !== undefined ? `?since=${since}` : ""}`),
  /** End the terminal a found session runs in and continue its conversation as an agent here. */
  /** `stop`: it runs outside kiln, so it is stopped on its machine first. */
  takeover: (machine: string, pid: number, opts?: { stop?: boolean }) =>
    call<AgentInfo[]>("POST", `/api/ai-sessions/${encodeURIComponent(machine)}/${pid}/takeover`, opts?.stop ? { stop: true } : undefined),
  /** Continue a copy of a found session's conversation as an agent; the session keeps running. */
  fork: (machine: string, pid: number) => call<AgentInfo[]>("POST", `/api/ai-sessions/${encodeURIComponent(machine)}/${pid}/fork`),
  start: (target: string, spec: Spec, count = 1) => call<AgentInfo[]>("POST", "/api/harness/sessions", { target, spec, count }),
  stop: (id: string) => call<void>("DELETE", `/api/harness/sessions/${encodeURIComponent(id)}`),
  /** A command without opening the session's socket (inbox answers). */
  command: (id: string, c: Command) => call<void>("POST", `/api/harness/sessions/${encodeURIComponent(id)}/command`, c),
  /** Upload a file for a later message (the agent's socket caps a message at 16 MB). */
  upload: async (blob: Blob, name: string, mime: string): Promise<string> => {
    const res = await fetch(`/api/harness/files?name=${encodeURIComponent(name)}&mime=${encodeURIComponent(mime)}`, {
      method: "POST",
      credentials: "same-origin",
      headers: { "content-type": mime || "application/octet-stream" },
      body: blob,
    });
    if (!res.ok) throw new Error(`${res.status} ${res.statusText}`);
    const j = (await res.json()) as { file_id?: string };
    if (!j.file_id) throw new Error("no file id");
    return j.file_id;
  },
  /** One command to many agents (each send gets its own message id). */
  broadcast: (ids: string[], c: Command) => call<{ sent: string[] }>("POST", "/api/harness/command", { ids, command: c }),
  /** One thing to many agents on every machine: a prompt or slash command, a command, or answers to what waits. */
  broadcastTo: (body: BroadcastBody) => call<BroadcastReport>("POST", "/api/harness/broadcast", body),
  /** Who a broadcast to `to` would reach, and their catalogs merged (each entry with how many have it). */
  broadcastPreview: (to: BroadcastTo) => call<BroadcastPreview>("POST", "/api/harness/broadcast/preview", { to }),
};

let seq = 0;
/** A client-made id for a message or request (delivery state is reported under it). */
export const clientId = () => `c${Date.now().toString(36)}${(++seq).toString(36)}${Math.random().toString(36).slice(2, 6)}`;

/* ------------------------------------------------------------------ live session */

export type Conn = "connecting" | "open" | "reconnecting" | "gone";

export interface Snapshot {
  conn: Conn;
  info: AgentInfo | null;
  meta: Meta | null;
  state: State | null;
  /** In timeline order. */
  events: HEvent[];
  /** The changeset, file by file (null until the server sends one). */
  changes: FileChange[] | null;
  error: string | null;
  /** Bumped on every change (cheap memo key). */
  version: number;
}

const EMPTY: Snapshot = { conn: "connecting", info: null, meta: null, state: null, events: [], changes: null, error: null, version: 0 };
const BACKOFF = [250, 500, 1000, 2000, 4000];

/**
 * One agent session, live: a websocket that receives the session's snapshot and then its updates,
 * and sends commands. Batches updates to one notification per animation frame, so a streaming
 * reply re-renders at most once a frame however fast it arrives.
 */
export class SessionStore {
  private snap: Snapshot = EMPTY;
  private byId = new Map<string, HEvent>();
  private order: HEvent[] | null = [];
  private listeners = new Set<() => void>();
  private ws: WebSocket | null = null;
  private stopped = false;
  private attempt = 0;
  private timer = 0;
  private raf = 0;
  private dirty = false;
  private replies = new Map<string, (v: unknown) => void>();

  constructor(readonly id: string) {}

  subscribe = (fn: () => void) => {
    this.listeners.add(fn);
    if (this.listeners.size === 1) this.connect();
    return () => {
      this.listeners.delete(fn);
      if (this.listeners.size === 0) this.close();
    };
  };

  get = () => this.snap;

  /** Send a command; false when the session isn't connected. */
  send = (c: Command): boolean => {
    if (this.ws?.readyState !== WebSocket.OPEN) return false;
    this.ws.send(JSON.stringify(c));
    return true;
  };

  /** Ask for something answered by a `reply` update (file suggestions ...). */
  request = (make: (client: string) => Command, timeoutMs = 4000): Promise<unknown> =>
    new Promise((resolve) => {
      const client = clientId();
      const t = window.setTimeout(() => {
        this.replies.delete(client);
        resolve(null);
      }, timeoutMs);
      this.replies.set(client, (v) => {
        window.clearTimeout(t);
        resolve(v);
      });
      if (!this.send(make(client))) {
        window.clearTimeout(t);
        this.replies.delete(client);
        resolve(null);
      }
    });

  private connect() {
    this.stopped = false;
    const ws = new WebSocket(wsUrl(`/api/harness/sessions/${encodeURIComponent(this.id)}/ws`));
    this.ws = ws;
    ws.onopen = () => {
      this.attempt = 0;
      this.patch({ conn: "open" });
    };
    ws.onmessage = (ev) => {
      if (typeof ev.data !== "string") return;
      let u: Update;
      try {
        u = JSON.parse(ev.data) as Update;
      } catch {
        return;
      }
      this.apply(u);
    };
    ws.onclose = () => {
      if (this.ws === ws) this.ws = null;
      if (this.stopped || this.snap.conn === "gone") return;
      this.patch({ conn: "reconnecting" });
      const delay = BACKOFF[Math.min(this.attempt, BACKOFF.length - 1)] ?? 4000;
      this.attempt += 1;
      this.timer = window.setTimeout(() => this.connect(), delay);
    };
  }

  private close() {
    this.stopped = true;
    window.clearTimeout(this.timer);
    cancelAnimationFrame(this.raf);
    this.ws?.close();
    this.ws = null;
  }

  private apply(u: Update) {
    switch (u.t) {
      case "open":
        // Always first, and again after the server resynced: the timeline starts over.
        this.byId.clear();
        this.order = [];
        this.snap = { ...this.snap, info: u.info, error: null };
        break;
      case "changes":
        this.snap = { ...this.snap, changes: u.files };
        break;
      case "error":
        this.snap = { ...this.snap, error: u.error };
        break;
      case "closed":
        this.snap = { ...this.snap, conn: "gone" };
        break;
      case "state":
        this.snap = { ...this.snap, state: u.state, info: this.snap.info ? { ...this.snap.info, status: u.state.status, title: u.state.title, model: u.state.model, mode: u.state.mode } : null };
        break;
      case "meta":
        this.snap = { ...this.snap, meta: u.meta };
        break;
      case "reset":
        this.byId.clear();
        this.order = [];
        break;
      case "events":
        for (const e of u.events) this.byId.set(e.id, e);
        this.order = null;
        break;
      case "stream": {
        const e = this.byId.get(u.id);
        if (e && (e.kind === "assistant" || e.kind === "thinking")) this.byId.set(u.id, { ...e, text: e.text + u.text });
        this.order = null;
        break;
      }
      case "reply": {
        const r = this.replies.get(u.client);
        this.replies.delete(u.client);
        r?.(u.value);
        return;
      }
    }
    this.schedule();
  }

  private patch(p: Partial<Snapshot>) {
    this.snap = { ...this.snap, ...p };
    this.schedule();
  }

  private schedule() {
    this.dirty = true;
    if (this.raf) return;
    this.raf = requestAnimationFrame(() => {
      this.raf = 0;
      if (!this.dirty) return;
      this.dirty = false;
      if (this.order === null) this.order = [...this.byId.values()].sort((a, b) => a.seq - b.seq);
      this.snap = { ...this.snap, events: this.order, version: this.snap.version + 1 };
      this.listeners.forEach((l) => l());
    });
  }
}

const stores = new Map<string, SessionStore>();
export function sessionStore(id: string): SessionStore {
  let s = stores.get(id);
  if (!s) {
    s = new SessionStore(id);
    stores.set(id, s);
  }
  return s;
}

/** A live agent session: its snapshot, re-rendered at most once per frame. */
export function useAgentSession(id: string): [Snapshot, SessionStore] {
  const store = React.useMemo(() => sessionStore(id), [id]);
  const snap = React.useSyncExternalStore(store.subscribe, store.get, store.get);
  return [snap, store];
}

/* ------------------------------------------------------------------ accounts */

/** A way to sign a harness in. `asks`: what it will want given back (the code the page shows, or an API key). */
export interface AccountMethod {
  id: string;
  label: string;
  asks?: "code" | "api_key";
}

/** Which account a harness runs as on one machine, as its own CLI reports it. */
export interface Account {
  /** "claude", "codex" … */
  kind: string;
  /** "Claude Code", "Codex" … */
  name: string;
  /** null: kiln couldn't tell (`error` says why). */
  signed_in: boolean | null;
  /** How it is signed in: "Claude subscription", "ChatGPT", "API key" … */
  method?: string;
  email?: string;
  org?: string;
  /** "max", "pro", "plus" … */
  plan?: string;
  /** The ways it can be signed in. */
  methods: AccountMethod[];
  error?: string;
}

/** A sign-in or sign-out, run by the harness's own CLI in a hidden terminal on its machine. */
export interface AccountFlow {
  id: string;
  target: string;
  kind: string;
  action: "login" | "logout";
  method?: string;
  state: "running" | "done" | "failed" | "cancelled";
  /** The sign-in page to open (on any device). */
  url?: string;
  /** A one-time code to enter on that page (device sign-in). */
  code?: string;
  /** Waiting to be given something: the code the page shows after signing in, or an API key. */
  asks?: "code" | "api_key";
  /** The CLI's own output, as plain text. */
  lines: string[];
  /** Once it ended: the account as it is now. */
  account?: Account;
  error?: string;
  /** Unix seconds. */
  started_at: number;
  ended_at?: number;
}

export const accountsApi = {
  /** Every harness's account on a machine ("local" or "agent:<machine>"). */
  list: (target: string) => call<Account[]>("GET", `/api/accounts?target=${encodeURIComponent(target)}`),
  login: (target: string, kind: string, method: string) => call<AccountFlow>("POST", "/api/accounts/login", { target, kind, method }),
  flow: (id: string) => call<AccountFlow>("GET", `/api/accounts/flows/${encodeURIComponent(id)}`),
  /** What a flow asks for (the page's code, or an API key): in the body only, never in a URL. */
  input: (id: string, text: string) => call<void>("POST", `/api/accounts/flows/${encodeURIComponent(id)}/input`, { text }),
  cancel: (id: string) => call<void>("DELETE", `/api/accounts/flows/${encodeURIComponent(id)}`),
  /** Signs out at once: the flow comes back finished, with the account as it is now. */
  logout: (target: string, kind: string) => call<AccountFlow>("POST", "/api/accounts/logout", { target, kind }),
};
