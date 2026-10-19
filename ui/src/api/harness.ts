/**
 * kiln as the harness: the normalized agent-session model (mirrors crates/kiln/src/harness/model.rs),
 * the API to list, start and stop agents, and a live session store fed by its websocket.
 * Nothing here is specific to one harness: Claude Code, Codex and the rest all speak this.
 */
import * as React from "react";
import { wsUrl } from "@/api/client";
import { events } from "@/api/events";
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

/** `by`: the harness that was running when it appeared (a conversation switched between harnesses keeps who said what). */
export type HEvent = { id: string; seq: number; at: number; parent?: string; by?: string } & Body;

/** An event found in a conversation: its agent, the name and machine the server knows it by (none: it ended and is forgotten), the event. */
export interface SaidHit {
  agent: string;
  name: string | null;
  machine: string | null;
  event: HEvent;
}

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
  /** Its name as a person last gave it (absent: the one it started with). */
  name?: string;
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
  /** Where its commands may run besides its machine: absent anywhere it needs, "off" only its own machine, else a runner or platform it prefers. */
  reach?: string | null;
  /** Which of those lists the harness has reported yet (servers that predate it omit this). */
  inventory?: { mcp?: Known; skills?: Known; plugins?: Known; limits?: Known };
  /** A switch to another harness's model, waiting for the turn under way to end. */
  switching?: { kind: string; model: string };
  /** Nothing more goes to the harness until this ends: its account reached a usage limit. Queued messages, goals and plan steps wait. */
  pause?: Pause;
  /** It pauses once a limit is this full (percent): between turns, or at the next step of one under way; absent: only when one is reached. */
  pause_at?: number;
  /** How its last turn ended (what kiln's keep-going reads). */
  last_turn?: TurnEnd;
  /** Turns in a row kiln started to keep it going, since a person last said something. */
  kept?: number;
  /** A pause ends only when a person resumes it. */
  manual_resume?: boolean;
  /** At a limit it goes on on another machine whose harness is signed in to an account with room. */
  move_on_limit?: boolean;
  /** Where it goes on then, as a person chose (sent by runners that take one, empty or not). */
  move_plan?: MovePlan;
}

/**
 * Where an agent goes on at a usage limit (`State.move_plan`): a machine (`agent:<name>`), and a
 * harness and model there. What isn't said kiln picks: whichever has room, its own harness or one
 * its conversation can be carried into.
 */
export interface MovePlan {
  to?: string;
  kind?: string;
  model?: string;
}

/** Why a session waits for a usage limit, and until when (`State.pause`). */
export interface Pause {
  /** "limit" (a limit was reached) or "threshold" (a limit got as full as `pause_at`). */
  reason: string;
  /** The window, as a `Limit.id`, when known. */
  window?: string;
  label?: string;
  /** What the harness said, as it said it. */
  said?: string;
  /** Unix ms the window starts over, when known. */
  until?: number;
  /** Unix ms it paused. */
  since: number;
  /** It goes on by itself once the limit resets. */
  auto: boolean;
  /** "harness" (it reached the limit), "account" (another agent on its account did) or "threshold". */
  by: string;
  /** The turn the limit cut off: it goes on first. */
  cut?: { message?: string; text?: string; at: number };
  /** Its folder as it was when it paused, kept in a hidden git ref. */
  checkpoint?: { git_ref: string; commit: string; at: number };
  /** Checks at the reset that found the limit still reached. */
  tries?: number;
  /** Unix ms a check started (waiting for the harness's answer). */
  checking?: number;
  /** Unix ms of the next check, when the reset time isn't known. */
  next_check?: number;
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
  /** The effort levels it takes, lowest first (when the harness says). */
  efforts?: string[];
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

/** One goal of an agent's goal stack. */
export interface GoalItem {
  id: string;
  text: string;
  status: "pending" | "active" | "met" | "unmet" | "skipped";
  reason?: string;
  /** How it went to the agent: its harness's /goal, its goal control, or a message. */
  how?: "goal" | "goal_control" | "message";
  started_at?: number;
  ended_at?: number;
  /** Times it was sent again, in a row, because the agent stopped short of it (no real work since). */
  reminders?: number;
  /** When it was last sent (unix ms). */
  reminded_at?: number;
}

/** An agent's goals, one after another: each goes once the one before is met. */
export interface GoalStack {
  agent: string;
  state: "running" | "paused" | "done";
  reason?: string;
  items: GoalItem[];
  updated_at: number;
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
  /** "steer", "send_now", "queue_cancel", "interrupt", "set_mode", "set_model", "stop_task", "file_suggestions", "resume", "switch_harness" */
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
/** How a turn ended (`State.last_turn`). */
export interface TurnEnd {
  at: number;
  /** "done", "cut" (stopped part-way), "stopped" (by you), "limit", "refused". */
  how: string;
  /** The harness's own word for it ("end_turn", "stop_sequence", "max_tokens" …). */
  why?: string;
  tools?: number;
  /** Its last step was a tool that failed, with nothing said after it. */
  after_error?: boolean;
  /** Its last words ask you something. */
  asks?: boolean;
  kept?: number;
}

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
  /** Where its commands may run besides its machine: absent anywhere it needs, "off" only its own machine, else a runner or platform it prefers. */
  reach?: string | null;
  /** It waits for a usage limit to reset (as `State.pause`). */
  pause?: Pause;
  /** Kiln doesn't keep it going when it stops part-way (the "keep_going" control). */
  keep_off?: boolean;
  /** At a limit it goes on on another machine with room (as `State.move_on_limit`). */
  move_on_limit?: boolean;
  /** Where it goes on then (as `State.move_plan`). */
  move_plan?: MovePlan;
  /** Where it pauses as its limits fill (as `State.pause_at`). */
  pause_at?: number;
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
  /** The agent that started it (docs/agents.md, "Agents that run agents"). */
  parent?: string;
  /** The plan run it does a step of. */
  plan_run?: string;
  /** Started without its harness's skip-every-check mode (an agent it starts can't have it either). */
  no_bypass?: boolean;
}

/** An agent's children that still run (and theirs), as `agents.children` gives them. */
export interface AgentChild {
  id: string;
  name: string;
  kind: string;
  status: Status;
  parent?: string;
}

export interface KindInfo {
  id: string;
  name: string;
  verified: boolean;
  available: boolean;
  /** Its permission modes; those that turn checks off are `dangerous` (runners before this left them out). */
  modes?: ModeOption[];
  default_mode?: string;
  /** Effort levels a new agent can start at, lowest first (older runners don't say). */
  efforts?: ControlOption[];
  /** The models it offers on that machine (older runners don't say). */
  models?: ModelOption[];
  /** An agent in another harness can go on in this one, its conversation carried over. */
  switch?: boolean;
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
  /** The effort level to start at (none: the model's own default). */
  effort?: string;
  /** Commands on other runners (kiln reach): unset, it may use any; "off", only its own machine; else where it prefers them (a runner, or `linux/x86_64`). */
  reach?: string;
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
  /** The turn didn't stop when asked: the harness starts again at once, the conversation going on. */
  | { type: "force_stop" }
  | { type: "set_mode"; mode: string }
  /** `kind`: the model is another harness's; the agent goes on in it (feature "switch_harness"). */
  | { type: "set_model"; model: string; kind?: string }
  | { type: "stop_task"; task: string }
  | { type: "stop_tasks" }
  /** Take a queued message out of the queue and steer it into the running turn (no interrupt). */
  | { type: "queue_steer"; id: string }
  | { type: "restart" }
  | { type: "suggest"; client: string; query: string }
  | { type: "snapshot" }
  /** End a pause now: the turn it cut off goes on, then the queue. */
  | { type: "resume" }
  /** Whether a pause ends by itself once its limit resets. */
  | { type: "auto_resume"; on: boolean }
  /** Pause once a limit is this full (null: only when one is reached). */
  | { type: "pause_at"; percent: number | null }
  /** Whether at a limit it goes on on another machine with room, and where (`MovePlan`). */
  | ({ type: "move_on_limit"; on: boolean } & MovePlan);

export type Update =
  | { t: "open"; info: AgentInfo }
  | { t: "changes"; files: FileChange[] }
  | { t: "error"; error: string }
  | { t: "closed" }
  | { t: "state"; state: State }
  /** What changed in the state since the last one (asked for with `?patch=state`): the fields set, and those gone. */
  | { t: "state_patch"; set: Partial<State>; gone?: string[] }
  | { t: "meta"; meta: Meta }
  | { t: "events"; events: HEvent[] }
  | { t: "stream"; id: string; text: string }
  | { t: "reply"; client: string; value: unknown }
  | { t: "reset" }
  /** The snapshot sent since "open" is complete (the server's socket says so; a machine resending its sessions doesn't). */
  | { t: "synced" };

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
  /** An agent's goal stack. */
  goals: (id: string) => call<GoalStack>("GET", `/api/harness/sessions/${encodeURIComponent(id)}/goals`),
  /** Its goals not yet started, as they should be (existing ones by id). */
  setGoals: (id: string, goals: { id?: string; text: string }[]) => call<GoalStack>("PUT", `/api/harness/sessions/${encodeURIComponent(id)}/goals`, { goals }),
  /** Goals after its others; `start` runs the stack. */
  addGoals: (id: string, goals: string[], start = false) => call<GoalStack>("POST", `/api/harness/sessions/${encodeURIComponent(id)}/goals`, { goals, start }),
  /** Start (or resume), pause, skip the goal under way, or clear. */
  goalAction: (id: string, action: "start" | "pause" | "skip" | "clear") => call<GoalStack>("POST", `/api/harness/sessions/${encodeURIComponent(id)}/goals/${action}`),
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
  /** End an agent for good; with `children`, the agents it started (and theirs) end first. */
  end: (id: string, children: boolean) => call<{ ended: string[] }>("POST", `/api/harness/agents/${encodeURIComponent(id)}/stop`, { children }),
  /** The agents an agent started that still run (and theirs). */
  children: (id: string) => call<{ children: AgentChild[] }>("GET", `/api/harness/agents/${encodeURIComponent(id)}/children`),
  /** Move an agent to another runner (`agent:<name>`): it goes on there once its turn is over. */
  /** Move an agent to runner `to`: once its turn is over, or `now` (the turn stops here and goes on there). */
  move: (id: string, to: string, now = false, kind?: string, model?: string) =>
    call<{ moving: string }>("POST", `/api/harness/sessions/${encodeURIComponent(id)}/move`, { to, now, ...(kind ? { kind } : {}), ...(model ? { model } : {}) }),
  /** Every agent of `machine`, which is gone, goes on elsewhere from the server's copy of its conversation: each on the machine that suits it, or all on `to`. */
  recover: (machine: string, to?: string) =>
    call<{ moved: { id: string; name: string; to?: string; error?: string }[] }>("POST", `/api/harness/machines/${encodeURIComponent(machine)}/recover`, to ? { to } : {}),
  /** A command without opening the session's socket (inbox answers). */
  command: (id: string, c: Command) => call<void>("POST", `/api/harness/sessions/${encodeURIComponent(id)}/command`, c),
  /** Events whose words hold every word of `q`, across conversations (one agent's with `agent`; those before `until`, unix ms), newest first, from the server's conversation store. */
  said: (p: { q: string; agent?: string; until?: number; limit?: number }) => {
    const qs = new URLSearchParams({ q: p.q });
    if (p.agent) qs.set("agent", p.agent);
    if (p.until !== undefined) qs.set("until", String(p.until));
    if (p.limit !== undefined) qs.set("limit", String(p.limit));
    return call<{ hits: SaidHit[] }>("GET", `/api/conversations/search?${qs}`);
  },
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
  /** The first snapshot is in: until then there's nothing to show but a loader. */
  ready: boolean;
  /** A stop asked of the turn under way: which turn, and when (ms). Done with once none runs. */
  stopAsked: { turn: number | undefined; at: number } | null;
}

/** A promise React reads as settled without waiting (its `status`, as React marks the ones it tracks). */
const READY: Promise<void> = Object.assign(Promise.resolve(), { status: "fulfilled", value: undefined });

const EMPTY: Snapshot = { conn: "connecting", info: null, meta: null, state: null, events: [], changes: null, error: null, ready: false, stopAsked: null };
const BACKOFF = [250, 500, 1000, 2000, 4000];
/**
 * The snapshot sent as the socket opens ends with the server's "synced"; one sent later (a machine
 * sending its sessions again) doesn't, and is taken as complete once its frames pause this long…
 */
const QUIET_MS = 300;
/** …or this long while none of its events came yet (a big first batch takes a while on a slow link). */
const QUIET_FIRST_MS = 1500;
/** What's shown is never held longer than this while a snapshot comes (a server without "synced": this long). */
const HOLD_MS = 5000;
/** A session the server doesn't know (yet) is tried this many times before it's taken as ended… */
const UNKNOWN_TRIES = 6;
/** …or, when it was here before (the server restarted and its machine isn't back yet), for this long. */
const UNKNOWN_KNOWN_MS = 60_000;
/** An ended session is looked for again this often, in case it was only away. */
const ENDED_RETRY_MS = 10_000;
/** A conversation left stays live this long: back in a moment, there's nothing to catch up on. */
const LINGER_MS = 30_000;
/** Conversations kept in memory once left (the latest ones). */
const KEEP = 8;
/**
 * The browser unpacks gzip itself (every current one does): a session's snapshot comes packed, a
 * long conversation's history in a tenth of the bytes.
 */
const GUNZIP = typeof DecompressionStream !== "undefined";
/** A packed message's text. */
const gunzip = (data: ArrayBuffer): Promise<string> => new Response(new Blob([data]).stream().pipeThrough(new DecompressionStream("gzip"))).text();

/**
 * What came, with every part equal to what was there kept as it was (the whole of it when it's all
 * equal), so whatever shows a part that didn't change doesn't render again.
 */
function share<T>(was: T, now: T): T {
  if (was === now || typeof was !== "object" || typeof now !== "object" || was === null || now === null) return now;
  if (Array.isArray(now)) {
    if (!Array.isArray(was)) return now;
    let equal = was.length === now.length;
    const out = now.map((x: unknown, i) => {
      const y = share(was[i] as unknown, x);
      if (y !== was[i]) equal = false;
      return y;
    });
    return (equal ? was : out) as T;
  }
  if (Array.isArray(was)) return now;
  const a = was as Record<string, unknown>;
  const b = now as Record<string, unknown>;
  const keys = Object.keys(b);
  let equal = keys.length === Object.keys(a).length;
  const out: Record<string, unknown> = {};
  for (const k of keys) {
    const y = share(a[k], b[k]);
    if (y !== a[k] || !Object.hasOwn(a, k)) equal = false;
    out[k] = y;
  }
  return (equal ? was : out) as T;
}

/** The agent's info with its state's words (a new name included), the same object while they're the same. */
const withState = (info: AgentInfo | null, s: State): AgentInfo | null => {
  const name = s.name || info?.name;
  return !info || (info.status === s.status && info.title === s.title && info.model === s.model && info.mode === s.mode && info.name === name)
    ? info
    : { ...info, name: name ?? info.name, status: s.status, title: s.title, model: s.model, mode: s.mode };
};

/** The state with a patch applied: what it sets, without what's gone, every other field the same object. */
const patched = (was: State | null, u: { set: Partial<State>; gone?: string[] }): State => {
  const now: Record<string, unknown> = { ...was, ...u.set };
  for (const k of u.gone ?? []) delete now[k];
  return now as unknown as State;
};

/** The socket's query: a snapshot packed when it can be unpacked here, and a state as what changed in it. */
const TAKES = GUNZIP ? "?z=gzip&patch=state" : "?patch=state";

/** A snapshot on its way: gathered here and swapped in at once, so what's shown never empties. */
interface Incoming {
  info: AgentInfo | null;
  meta: Meta | null;
  state: State | null;
  changes: FileChange[] | null;
  byId: Map<string, HEvent>;
  /** Some of its events came. */
  events: boolean;
  /** It ends with the server's "synced". */
  told: boolean;
}

/**
 * One agent session, live: a websocket that receives the session's snapshot and then its updates,
 * and sends commands.
 * - A snapshot (on connect, after a reconnect, when its machine came back) is gathered while what's
 *   shown stays, and swapped in at once when it's complete; what didn't change keeps its objects,
 *   so nothing re-renders for it and nothing moves.
 * - Updates are batched to one notification per animation frame, so a streaming reply re-renders
 *   at most once a frame however fast it arrives.
 * - It reconnects by itself, at once when the server's event stream says the server or the
 *   session is back. A session the server doesn't know right now (it restarted and the session's
 *   machine hasn't reconnected yet) is tried again rather than taken as ended.
 * - Left, it stays live a while (coming back shows it as it is, at once), then closes and keeps
 *   what it had for the next visit.
 */
export class SessionStore {
  private snap: Snapshot = EMPTY;
  private byId = new Map<string, HEvent>();
  /** Where each event is in `snap.events`. */
  private pos = new Map<string, number>();
  /** Since the last notification: events replaced, new ones, a new snapshot. */
  private touched = new Set<string>();
  private grew = false;
  /** Ids of the events new since the last notification, as they came. */
  private fresh: string[] = [];
  private reorder = false;
  private incoming: Incoming | null = null;
  private listeners = new Set<() => void>();
  private ws: WebSocket | null = null;
  private wsAt = 0;
  /** Frames on this connection so far. */
  private frames = 0;
  /** Something shows it (or will in a moment): the socket stays up. */
  private wanted = false;
  private unhook: (() => void)[] = [];
  /** This connection has said "open": a "closed" before it means the server doesn't know the session now. */
  private opened = false;
  /** It was here before. */
  private known = false;
  /** The server said it ended. */
  private ended = false;
  private misses = 0;
  private missingSince = 0;
  private attempt = 0;
  private timer = 0;
  private quiet = 0;
  private hold = 0;
  private linger = 0;
  private raf = 0;
  private dirty = false;
  /** When nothing showed it any more. */
  idleSince = 0;
  private replies = new Map<string, (v: unknown) => void>();
  private waiting: { promise: Promise<void>; resolve: () => void } | null = null;
  /** Packed messages being unpacked, and the chain they (and what came after them) are taken in. */
  private unpacking = 0;
  private inbox: Promise<void> = Promise.resolve();

  constructor(readonly id: string) {}

  subscribe = (fn: () => void) => {
    this.listeners.add(fn);
    window.clearTimeout(this.linger);
    this.want();
    return () => {
      this.listeners.delete(fn);
      if (this.listeners.size === 0) this.leave();
    };
  };

  get = () => this.snap;

  /**
   * Resolves once the first snapshot is in (what Suspense waits on); connects if nothing has yet.
   * The same promise while it waits, and one already settled after (React reads it at once).
   */
  ready(): Promise<void> {
    if (this.snap.ready) return READY;
    this.want();
    // Nothing shows it yet: it closes again unless something does soon.
    if (this.listeners.size === 0) this.leave();
    if (!this.waiting) {
      let resolve = () => {};
      const promise = new Promise<void>((r) => (resolve = r));
      this.waiting = { promise, resolve };
    }
    return this.waiting.promise;
  }

  /** Nothing shows it and nothing will: it can be dropped. */
  idle = () => !this.wanted && this.listeners.size === 0;

  /** Connect now rather than at the next try (the tab is shown again, the network is back). */
  wake = () => this.kick(false);

  /** Send a command; false when the session isn't connected. */
  send = (c: Command): boolean => {
    if (this.ws?.readyState !== WebSocket.OPEN) return false;
    this.ws.send(JSON.stringify(c));
    return true;
  };

  /**
   * Stop the turn under way; `force` when asking didn't take (the harness starts again, the
   * conversation going on). Said over the socket, else over HTTP: a stop mustn't wait for a
   * reconnect. An ask is remembered, so every stop control shows it and can offer to force it.
   */
  stop = (force = false) => {
    const c: Command = force ? { type: "force_stop" } : { type: "interrupt" };
    if (!force) this.patch({ stopAsked: { turn: this.snap.state?.turn_started, at: Date.now() } });
    if (!this.send(c)) harnessApi.command(this.id, c).catch(() => this.patch({ error: "Couldn't reach the server to stop the turn; try again" }));
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

  private want() {
    if (this.wanted) return;
    this.wanted = true;
    this.unhook = [
      // The server restarted, or the network came back: this socket may be gone too without having said so.
      events.onReopen(() => this.kick(true)),
      // The session is on the server again (its machine came back): no need to wait for the next try.
      events.on((ev) => {
        if (ev.type === "harness" && ev.info.id === this.id) this.kick(false);
      }),
    ];
    this.connect();
  }

  /** Nothing shows it any more: it stays live a while, then closes. */
  private leave() {
    window.clearTimeout(this.linger);
    this.linger = window.setTimeout(() => {
      if (this.listeners.size === 0) this.close();
    }, LINGER_MS);
  }

  private close() {
    this.wanted = false;
    this.unhook.forEach((f) => f());
    this.unhook = [];
    window.clearTimeout(this.timer);
    window.clearTimeout(this.quiet);
    window.clearTimeout(this.hold);
    // A snapshot half in is dropped: the next connection sends it again.
    this.incoming = null;
    cancelAnimationFrame(this.raf);
    this.raf = 0;
    this.flush();
    const ws = this.ws;
    this.ws = null;
    ws?.close();
    this.idleSince = Date.now();
    // Whatever still waits for its first snapshot renders again (and connects again if it's still shown).
    this.settle();
    prune();
  }

  private connect() {
    window.clearTimeout(this.timer);
    this.timer = 0;
    if (!this.wanted) return;
    const old = this.ws;
    this.ws = null;
    old?.close();
    this.opened = false;
    this.frames = 0;
    this.incoming = null;
    let ws: WebSocket;
    try {
      ws = new WebSocket(wsUrl(`/api/harness/sessions/${encodeURIComponent(this.id)}/ws${TAKES}`));
    } catch {
      this.retry();
      return;
    }
    ws.binaryType = "arraybuffer";
    this.ws = ws;
    this.wsAt = Date.now();
    this.unpacking = 0;
    ws.onmessage = (ev) => {
      if (this.ws !== ws) return;
      if (typeof ev.data === "string") {
        this.receive(ws, ev.data);
        return;
      }
      // A packed snapshot (`?z=gzip`): unpacked, then its lines taken in order; what comes after
      // it waits its turn.
      const data = ev.data as ArrayBuffer;
      this.unpacking += 1;
      this.inbox = this.inbox
        .then(() => gunzip(data))
        .then((text) => {
          if (this.ws === ws) for (const line of text.split("\n")) if (line) this.take1(line);
        })
        .catch(() => undefined)
        .finally(() => {
          this.unpacking -= 1;
        });
    };
    ws.onclose = () => {
      if (this.ws !== ws) return;
      this.ws = null;
      this.lost();
    };
  }

  /** A message as it came: at once, unless a packed one before it is still being unpacked. */
  private receive(ws: WebSocket, data: string) {
    if (this.unpacking === 0) {
      this.take1(data);
      return;
    }
    this.inbox = this.inbox.then(() => {
      if (this.ws === ws) this.take1(data);
    });
  }

  /** One update, as JSON. */
  private take1(data: string) {
    let u: Update;
    try {
      u = JSON.parse(data) as Update;
    } catch {
      return;
    }
    this.frames += 1;
    this.apply(u);
  }

  /** The socket closed: try again (rarely once it has ended, in case it was only away). */
  private lost() {
    if (!this.wanted) return;
    window.clearTimeout(this.quiet);
    window.clearTimeout(this.hold);
    this.incoming = null;
    if (this.ended) {
      this.retry(ENDED_RETRY_MS);
      return;
    }
    if (this.snap.conn === "open") this.patch({ conn: "reconnecting" });
    this.retry();
  }

  private retry(ms?: number) {
    window.clearTimeout(this.timer);
    const delay = ms ?? BACKOFF[Math.min(this.attempt, BACKOFF.length - 1)] ?? 4000;
    this.attempt += 1;
    this.timer = window.setTimeout(() => this.connect(), delay);
  }

  /** Connect now. `fresh`: even over a socket that looks open (it may have died without saying so). */
  private kick(fresh: boolean) {
    if (!this.wanted) return;
    const ws = this.ws;
    if (ws && ws.readyState === WebSocket.CONNECTING) return;
    if (ws && ws.readyState === WebSocket.OPEN && (!fresh || Date.now() - this.wsAt < 2000)) return;
    this.attempt = 0;
    this.connect();
  }

  private apply(u: Update) {
    switch (u.t) {
      case "reply": {
        const r = this.replies.get(u.client);
        this.replies.delete(u.client);
        r?.(u.value);
        return;
      }
      case "error":
        this.patch({ error: u.error });
        return;
      case "open":
        this.opened = true;
        this.ended = false;
        this.misses = 0;
        this.attempt = 0;
        // Sent as the socket opens: the server's own snapshot, which it ends with "synced".
        this.begin(u.info, this.frames === 1);
        if (this.snap.ready && this.snap.conn !== "open") this.patch({ conn: "open" });
        return;
      case "synced":
        this.commit();
        return;
      case "closed":
        if (!this.opened) {
          this.unknown();
          return;
        }
        this.commit();
        this.ended = true;
        this.patch({ conn: "gone", ready: true });
        this.settle();
        return;
    }
    const inc = this.incoming;
    if (inc) {
      // A snapshot holds nothing after its events but more events: anything else is live, so it's complete.
      if (u.t !== "stream" && !(inc.events && u.t !== "events")) {
        this.gather(inc, u);
        return;
      }
      this.commit();
    }
    this.take(u);
    this.schedule();
  }

  private begin(info: AgentInfo | null, told: boolean) {
    window.clearTimeout(this.hold);
    this.incoming = { info, meta: this.snap.meta, state: this.snap.state, changes: null, byId: new Map(), events: false, told };
    this.hold = window.setTimeout(() => this.commit(), HOLD_MS);
    this.wait();
  }

  private gather(inc: Incoming, u: Update) {
    switch (u.t) {
      case "reset":
        inc.byId.clear();
        break;
      case "meta":
        inc.meta = u.meta;
        break;
      case "state":
      case "state_patch":
        inc.state = u.t === "state" ? u.state : patched(inc.state, u);
        inc.info = withState(inc.info, inc.state);
        break;
      case "changes":
        inc.changes = u.files;
        break;
      case "events":
        inc.events = true;
        for (const e of u.events) inc.byId.set(e.id, e);
        break;
    }
    this.wait();
  }

  /** Without "synced", the snapshot is taken as complete once its frames pause. */
  private wait() {
    window.clearTimeout(this.quiet);
    const inc = this.incoming;
    if (!inc || inc.told) return;
    this.quiet = window.setTimeout(() => this.commit(), inc.events ? QUIET_MS : QUIET_FIRST_MS);
  }

  /** The snapshot is complete: it replaces what's shown in one change. */
  private commit() {
    const inc = this.incoming;
    if (!inc) return;
    this.incoming = null;
    window.clearTimeout(this.quiet);
    window.clearTimeout(this.hold);
    const byId = new Map<string, HEvent>();
    for (const [id, e] of inc.byId) {
      const was = this.byId.get(id);
      byId.set(id, was ? share(was, e) : e);
    }
    this.byId = byId;
    this.reorder = true;
    const s = this.snap;
    this.snap = {
      ...s,
      conn: "open",
      info: share(s.info, inc.info),
      meta: share(s.meta, inc.meta),
      state: share(s.state, inc.state),
      changes: share(s.changes, inc.changes),
      error: null,
      ready: true,
    };
    // Ready only with its events in order, so the first render has them all.
    this.snap = { ...this.snap, events: this.ordered() };
    this.known = true;
    this.settle();
    this.schedule();
  }

  /** The server doesn't know the session right now: it restarted and the session's machine isn't back yet, or the session ended. */
  private unknown() {
    // Ended already: it stays so (and is looked for now and then).
    if (this.ended) return;
    this.misses += 1;
    if (this.misses === 1) this.missingSince = Date.now();
    const away = this.known ? Date.now() - this.missingSince < UNKNOWN_KNOWN_MS : this.misses < UNKNOWN_TRIES;
    if (away) {
      // Most likely back in a moment: what's shown stays (or the loader does), and it's tried again.
      if (this.snap.ready && this.snap.conn !== "reconnecting") this.patch({ conn: "reconnecting" });
      return;
    }
    // Taken as ended; still looked for now and then.
    this.ended = true;
    this.patch({ conn: "gone", ready: true });
    this.settle();
  }

  private take(u: Update) {
    switch (u.t) {
      case "changes":
        this.snap = { ...this.snap, changes: share(this.snap.changes, u.files) };
        break;
      case "state":
      case "state_patch": {
        const now = u.t === "state" ? u.state : patched(this.snap.state, u);
        const state = share<State | null>(this.snap.state, now) ?? now;
        // A stop asked of a turn holds while one runs; once none does, it's done with.
        const stopAsked = state.status === "running" || state.status === "waiting" ? this.snap.stopAsked : null;
        this.snap = { ...this.snap, state, info: withState(this.snap.info, state), stopAsked };
        break;
      }
      case "meta":
        this.snap = { ...this.snap, meta: share(this.snap.meta, u.meta) };
        break;
      case "events":
        for (const e of u.events) {
          const was = this.byId.get(e.id);
          if (!was) {
            this.grew = true;
            this.fresh.push(e.id);
            this.byId.set(e.id, e);
            continue;
          }
          const now = share(was, e);
          if (now === was) continue;
          this.byId.set(e.id, now);
          this.touched.add(e.id);
        }
        break;
      case "stream": {
        const e = this.byId.get(u.id);
        if (e && (e.kind === "assistant" || e.kind === "thinking")) {
          this.byId.set(u.id, { ...e, text: e.text + u.text });
          this.touched.add(u.id);
        }
        break;
      }
      case "reset":
        // Only ever the start of a snapshot (after "open"); taken as one all the same.
        this.begin(this.snap.info, false);
        break;
    }
  }

  private settle() {
    const w = this.waiting;
    this.waiting = null;
    w?.resolve();
  }

  /**
   * The events in timeline order: the same array while they are, and the same objects. What's new
   * almost always comes after the last one, and joins at the end as it is (sorting thousands of
   * events for each new one stalled long conversations); only one that doesn't sorts them all.
   */
  private ordered(): HEvent[] {
    const was = this.snap.events;
    if (this.grew && !this.reorder) {
      const add = this.fresh.map((id) => this.byId.get(id)).filter((e): e is HEvent => e !== undefined);
      add.sort((a, b) => a.seq - b.seq);
      const last = was.length ? (was[was.length - 1]?.seq ?? -Infinity) : -Infinity;
      if ((add[0]?.seq ?? Infinity) > last) {
        const next = was.slice();
        for (const id of this.touched) {
          const i = this.pos.get(id);
          const e = this.byId.get(id);
          if (i !== undefined && e) next[i] = e;
        }
        for (const e of add) {
          this.pos.set(e.id, next.length);
          next.push(e);
        }
        this.grew = false;
        this.fresh = [];
        this.touched.clear();
        return next;
      }
    }
    if (this.reorder || this.grew) {
      const next = [...this.byId.values()].sort((a, b) => a.seq - b.seq);
      this.reorder = false;
      this.grew = false;
      this.fresh = [];
      this.touched.clear();
      const out = next.length === was.length && next.every((e, i) => e === was[i]) ? was : next;
      this.pos = new Map(out.map((e, i) => [e.id, i]));
      return out;
    }
    if (this.touched.size === 0) return was;
    const next = was.slice();
    for (const id of this.touched) {
      const i = this.pos.get(id);
      const e = this.byId.get(id);
      if (i !== undefined && e) next[i] = e;
    }
    this.touched.clear();
    return next;
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
      this.flush();
    });
  }

  private flush() {
    if (!this.dirty) return;
    this.dirty = false;
    const events = this.ordered();
    if (events !== this.snap.events) this.snap = { ...this.snap, events };
    this.listeners.forEach((l) => l());
  }
}

const stores = new Map<string, SessionStore>();

/** Drops the conversations left longest ago beyond the few kept. */
function prune() {
  const idle = [...stores.values()].filter((s) => s.idle()).sort((a, b) => b.idleSince - a.idleSince);
  for (const s of idle.slice(KEEP)) stores.delete(s.id);
}

export function sessionStore(id: string): SessionStore {
  let s = stores.get(id);
  if (!s) {
    s = new SessionStore(id);
    stores.set(id, s);
  }
  return s;
}

// Back at once when the tab is shown again or the network returns, not at the next try.
if (typeof window !== "undefined") {
  const wake = () => stores.forEach((s) => s.wake());
  window.addEventListener("online", wake);
  document.addEventListener("visibilitychange", () => {
    if (!document.hidden) wake();
  });
}

/**
 * A part of an agent's live session; re-renders only when that part changes. `pick` returns a part
 * as it is (`s => s.state`) or a plain value (`s => s.state?.status`), never a new object.
 */
export function useSession<T>(store: SessionStore, pick: (s: Snapshot) => T): T {
  return React.useSyncExternalStore(
    store.subscribe,
    () => pick(store.get()),
    () => pick(store.get()),
  );
}

/** How long a stop has to take before it can be forced. */
export const FORCE_STOP_AFTER_MS = 8000;

/** When a stop was asked of the turn under way (ms), or null. */
export function useStopAt(store: SessionStore): number | null {
  return useSession(store, (s) => (s.stopAsked && s.stopAsked.turn === s.state?.turn_started ? s.stopAsked.at : null));
}

/** Waits (Suspense) until the session's first snapshot is in. */
export function useSessionReady(store: SessionStore) {
  // Read on every render (settled once it's in), as React expects of what a render suspended on.
  useSession(store, (s) => s.ready);
  React.use(store.ready());
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
