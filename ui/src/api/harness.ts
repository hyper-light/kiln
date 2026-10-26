/**
 * kiln as the harness: the normalized agent-session model (mirrors crates/kiln/src/harness/model.rs),
 * the API to list, start and stop agents, and a live session store fed by its websocket.
 * Nothing here is specific to one harness: Claude Code, Codex and the rest all speak this.
 */
import * as React from "react";
import { wsUrl } from "@/api/client";
import { apiPath } from "@/lib/peer-base";
import { events } from "@/api/events";
import type { TerminalSessionInfo } from "./types";
import type {
  Account,
  AccountFlow,
  AgentInfo,
  Attachment,
  Command,
  ControlOption,
  FileChange,
  Event as HEvent,
  KeptFile,
  Meta,
  ModelOption,
  ModeOption,
  SendMode,
  SessionDown,
  SessionUp,
  SlashCommand,
  AgentState as State,
  SessionStatus as Status,
} from "./wire";
export type {
  Account,
  AccountFlow,
  AccountMethod,
  AgentInfo,
  AlwaysOption,
  Approval,
  ApprovalBrief,
  Attachment,
  ChoiceAnswer,
  Command,
  Compacting,
  Control,
  ControlOption,
  Decision,
  Delivery,
  ExitInfo,
  FileChange,
  FileDiff,
  FileMeta,
  HeldGoals as Held,
  Event as HEvent,
  HookInfo,
  Hunk,
  JsonValue,
  KeptFile,
  Known,
  Level,
  Limit,
  McpServer,
  Meta,
  ModelOption,
  ModeOption,
  MovePlan,
  Pause,
  Plugin,
  Question,
  QuestionOption,
  QueueItem,
  SendMode,
  SessionDown,
  SessionUp,
  Skill,
  SlashCommand,
  AgentState as State,
  SessionStatus as Status,
  StatusItem,
  StreamDown,
  StreamUp,
  SubagentBrief,
  Task,
  Todo,
  ToolCall,
  ToolKind,
  ToolStatus,
  TurnEnd,
  Usage,
  WebrtcDown,
  WebrtcUp,
} from "./wire";

/* ------------------------------------------------------------------ model */

/** An event found in a conversation: its agent, the name and machine the server knows it by (none: it ended and is forgotten), the event. */
export interface SaidHit {
  agent: string;
  name: string | null;
  machine: string | null;
  event: HEvent;
}

/** A slice of a task's output (`since` → `offset`); `gone` once the harness deleted it. */
export interface TaskOutput {
  data?: string;
  /** A transcript snapshot replaces previously read output instead of appending to it. */
  reset?: boolean;
  pending?: boolean;
  offset?: number;
  live?: boolean;
  truncated?: boolean;
  path?: string;
  gone?: boolean;
  error?: string;
}

/** One file's change since the agent first touched it (the server keeps it cumulative). */

/** A conversation's kept files with these added (each path and message once). */
function withFiles(was: KeptFile[], added: KeptFile[]): KeptFile[] {
  const fresh = added.filter((a) => !was.some((w) => w.path === a.path && w.seq === a.seq));
  return fresh.length ? [...was, ...fresh] : was;
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
  /** The goal it's a part of (split into parts: that one comes back after them). */
  part_of?: string;
  /** The plan step it was made from (`n` from 1). */
  step?: { plan: string; version: number; n: number; run?: string };
  /** The agent it's under way on, handed to it (a program's coordinator's goal, its track's worker's: docs/programs.md). */
  on?: string;
  /** Who made it a part: the agent (`own`: its goal comes back for its own check), or kiln (`checks`, `plan`, `planning`, `run`: its goal is met once they all are). */
  part_kind?: "own" | "checks" | "plan" | "planning" | "run";
  /** The plan it became. */
  planned_as?: string;
  /** The plan run it waits for. */
  waits_on?: string;
  /** The subagent a plan's step was handed to, side by side with the plan's other steps (docs/goals.md, "A plan's steps side by side"): under way there while it's still to go here; once met, who met it. */
  subagent?: string;
}

/** An agent's goals, one after another: each goes once the one before is met. */
export interface GoalStack {
  agent: string;
  state: "running" | "paused" | "done";
  reason?: string;
  items: GoalItem[];
  updated_at: number;
  /** How many goals the last clear kept, to bring back (`restore`). */
  cleared?: number;
  /** Paused for a while: when it resumes by itself (unix ms). */
  resume_at?: number;
  /** Its big goals planned first (docs/goals.md, "Big goals, planned first"): on unless turned off. */
  plan_big?: boolean;
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
  /** Files with the prompt (uploaded first), each agent its own copy. */
  attachments?: Attachment[];
  mode?: SendMode;
  command?: Command;
  approvals?: "allow" | "deny";
  /** An idempotency key: sent again with it, each agent's message is the same one, delivered once. */
  key?: string;
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

/** An entry of the agents' merged catalogs: how many of them have it, and which. */
type Counted<T> = T & { count: number; agents?: string[] };

export interface BroadcastPreview {
  agents: { id: string; name: string; machine: string; kind: string; status: Status }[];
  machines: string[];
  commands: Counted<SlashCommand>[];
  modes: Counted<ModeOption>[];
  models: Counted<ModelOption>[];
  efforts: Counted<ControlOption>[];
}

/** An agent session as lists show it. */

/** What writing an agent's workspace back to its folder would do (`agents.land` without a landing). */
export interface LandingPlan {
  plan: {
    grant_required: boolean;
    landing?: number;
    manifest?: string;
    summary?: { by_action: { action: string; count: number }[]; bytes: number; filtered_out: number };
    conflicts?: string[];
    outcome?: unknown;
  };
  /** Left out, from the top: git's own directory. */
  excludes: string[];
  /** Whether this machine's Slates can take a person's go-ahead from kiln (not on Linux yet), and why not. */
  can_write: boolean;
  why_not?: string;
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
  /** Where it works: its folder itself (none), or a Slates volume of its own laid over the folder (`volume`). */
  workspace?: { kind: "volume" };
}

/** Where agents can have workspaces of their own, by target (`local`, `agent:<name>`). */
export type WorkspacesAvailable = Record<string, { available: boolean; why_not?: string }>;

/* ------------------------------------------------------------------ REST */

async function call<T>(method: string, path: string, body?: unknown, signal?: AbortSignal, headers?: Record<string, string>): Promise<T> {
  const res = await fetch(path, {
    method,
    credentials: "same-origin",
    headers: body === undefined ? headers : { "content-type": "application/json", ...headers },
    body: body === undefined ? undefined : JSON.stringify(body),
    signal,
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

/** A port an agent's machine listens on (`crate::browser::apps::Listening`). */
export interface Listening {
  port: number;
  address: string;
  pid?: number;
  command: string;
  /** One of the session's own processes. */
  agents: boolean;
}

/** A device a live page can be laid out as (`crate::browser::devices`). */
export interface Device {
  name: string;
  width: number;
  height: number;
  scale: number;
  mobile: boolean;
  agent?: string;
}

/** What a person may open live from a session (`GET .../preview`): its machine's ports, its
 *  project's app, the engines it has, the devices kiln knows. */
export interface PreviewLook {
  ports: Listening[];
  app: { configured: boolean; start?: string; port?: number; path?: string; running?: string | null; error?: string };
  engines?: { chromium?: boolean; firefox?: boolean; webkit?: boolean; ios?: boolean };
  devices?: Device[];
  /** The machine's system (`macos`, `linux`, `windows`). */
  platform?: string;
  /** Whether the one asking may open pages in a profile of their own, and why not. */
  personal?: { ok: boolean; why?: string | null };
}

/** A live page of a session, as the server relays it (`crate::harness::streams::Live::view`). */
export interface StreamView {
  id: string;
  session: string;
  machine: string;
  spec: { url?: string; title?: string; profile?: string; engine?: string; device?: string };
  by: string;
  at: number;
  viewers: number;
  said: { opened?: unknown; navigated?: { url?: string }; loaded?: { title?: string } };
}

/** An agent's app opened on this person's own devices (`crate::harness::opened`): `url` (when
 *  just opened) its link, good once for ten minutes. */
export interface Opened {
  id: string;
  session: string;
  port: number;
  path: string;
  by: string;
  at: number;
  until: number;
  url?: string;
}

/** A recording of one of a session's streams (`crate::harness::streams::Recording`): its video,
 *  its log of what was done, and that as a Playwright test, on its machine. */
export interface Recording {
  id: string;
  stream: string;
  at: number;
  seconds: number;
  bytes: number;
  pictures: number;
  video: string;
  inputs: string;
  test: string;
}

/** Where a recording's file is read (`video`, `inputs`, `test`). */
export const recordingUrl = (session: string, id: string, what: "video" | "inputs" | "test") => apiPath(`/api/harness/sessions/${encodeURIComponent(session)}/recordings/${id}/${what}`);

export const harnessApi = {
  kinds: (target: string) => call<KindInfo[]>("GET", `/api/harness/kinds?target=${encodeURIComponent(target)}`),
  /** What a person may open live from it: the ports its machine serves (its agent's first), its project's app. */
  preview: (id: string) => call<PreviewLook>("GET", `/api/harness/sessions/${encodeURIComponent(id)}/preview`),
  /** Stop the project's app kiln started. */
  stopApp: (id: string) => call<{ stopped: boolean }>("POST", `/api/harness/sessions/${encodeURIComponent(id)}/preview/stop`),
  /** Its live pages. */
  streams: (id: string) => call<StreamView[]>("GET", `/api/harness/sessions/${encodeURIComponent(id)}/streams`),
  /** Open a live page: an address, or (`start`) the project's app; in a profile. */
  openStream: (
    id: string,
    body: { url?: string; start?: boolean; profile?: string; engine?: string; device?: string; width?: number; height?: number; scale?: number; mobile?: boolean; title?: string },
  ) => call<StreamView>("POST", `/api/harness/sessions/${encodeURIComponent(id)}/streams`, body),
  closeStream: (id: string, stream: string) => call<void>("DELETE", `/api/harness/sessions/${encodeURIComponent(id)}/streams/${stream}`),
  /** Open the app on a port of its machine on this person's own device: a link to follow there. */
  openOnDevice: (id: string, body: { port: number; path?: string }) => call<Opened>("POST", `/api/harness/sessions/${encodeURIComponent(id)}/opened`, body),
  /** This person's apps of it open on their devices. */
  opened: (id: string) => call<Opened[]>("GET", `/api/harness/sessions/${encodeURIComponent(id)}/opened`),
  endOpened: (id: string, opened: string) => call<void>("DELETE", `/api/harness/sessions/${encodeURIComponent(id)}/opened/${opened}`),
  /** Recordings of its streams. */
  recordings: (id: string) => call<Recording[]>("GET", `/api/harness/sessions/${encodeURIComponent(id)}/recordings`),
  /** Every connected machine's harnesses, by target ("local", "agent:<name>"). */
  kindsAll: () => call<Record<string, KindInfo[]>>("GET", "/api/harness/kinds"),
  /** Where agents can have workspaces of their own (Slates volumes), by target. */
  workspaces: () => call<WorkspacesAvailable>("GET", "/api/harness/workspaces"),
  /** An agent's workspace of its own, and the files under it changed on disk since it changed them. */
  workspace: (id: string) => call<{ workspace: AgentInfo["workspace"] | null; drift: string[] }>("GET", `/api/harness/agents/${encodeURIComponent(id)}/workspace`),
  /** What writing its workspace to its folder would do. */
  planLanding: (id: string) => call<LandingPlan>("POST", `/api/harness/agents/${encodeURIComponent(id)}/land`, {}),
  /** Write it exactly as the plan whose manifest this is (a person's go-ahead); if it changed since, nothing is written and the new plan comes back. */
  land: (id: string, manifest: string) => call<{ landed?: unknown } & Partial<LandingPlan> & { changed?: boolean }>("POST", `/api/harness/agents/${encodeURIComponent(id)}/land`, { manifest }),
  list: () => call<AgentInfo[]>("GET", "/api/harness/sessions"),
  /** An agent's goal stack. */
  goals: (id: string) => call<GoalStack>("GET", `/api/harness/sessions/${encodeURIComponent(id)}/goals`),
  /** Its goals still to go, as they should be (existing ones by id; the one under way among them puts it back or rewords it). */
  setGoals: (id: string, goals: { id?: string; text: string }[]) => call<GoalStack>("PUT", `/api/harness/sessions/${encodeURIComponent(id)}/goals`, { goals }),
  /** Goals after its others; `start` runs the stack. */
  /** `key` (CP-T3.2): asked again under it, its goals are added once. */
  addGoals: (id: string, goals: string[], start = false, key?: string) => call<GoalStack>("POST", `/api/harness/sessions/${encodeURIComponent(id)}/goals`, { goals, start, key }),
  /** Start (or resume), pause, skip the goal under way, clear, or bring back the goals cleared last. */
  /** `goal`: for a skip, the goal under way it's for (asked again once it's skipped, nothing more is: CP-T3.2). */
  /** `key` (CP-T3.2): asked again under it (its answer lost), it does nothing more, whatever changed since. */
  goalAction: (id: string, action: "start" | "pause" | "skip" | "clear" | "restore", goal: string | undefined, key: string) =>
    call<GoalStack>("POST", `/api/harness/sessions/${encodeURIComponent(id)}/goals/${action}`, goal ? { id: goal, key } : { key }),
  /** One of its goals turned into a plan: planning it (research, its steps, ready) goes first, then its steps (`goals_plan`). */
  /** Whether its stack plans its big goals first (a person's setting, not the agent's). */
  goalSettings: (id: string, settings: { plan_big: boolean }) => call<GoalStack>("POST", `/api/harness/sessions/${encodeURIComponent(id)}/goals/settings`, settings),
  /** `dispatched`: a subagent of its agent plans it now (else `why` not: its planning waits its turn). */
  planGoal: (id: string, goal: string) =>
    call<{ plan: string; research: string; goal: string; part: string; dispatched?: boolean; why?: string | null }>("POST", `/api/harness/sessions/${encodeURIComponent(id)}/goals/plan`, { id: goal }),
  /** The terminals beside an agent (its shell, its harness's own screen), live ones first. */
  terminals: (id: string) => call<BesideTerminal[]>("GET", `/api/harness/sessions/${encodeURIComponent(id)}/terminals`),
  /** Open one beside it: its shell (the one already open, with `command` run in it), or its harness's own screen on `command`. */
  openTerminal: (id: string, body: { what: "shell" | "screen"; command?: string; cols?: number; rows?: number }) =>
    call<BesideTerminal>("POST", `/api/harness/sessions/${encodeURIComponent(id)}/terminals`, body),
  /** A machine's recent ended conversations (newest first), for one harness or all. */
  history: (target: string, kind?: string, limit = 40) =>
    call<PastSession[]>("GET", `/api/harness/history?target=${encodeURIComponent(target)}${kind ? `&kind=${encodeURIComponent(kind)}` : ""}&limit=${limit}`),
  /** A task's output: its tail, or what came after byte `since`. */
  taskOutput: (id: string, task: string, since?: number, signal?: AbortSignal) =>
    call<TaskOutput>("GET", `/api/harness/sessions/${encodeURIComponent(id)}/tasks/${encodeURIComponent(task)}/output${since !== undefined ? `?since=${since}` : ""}`, undefined, signal),
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
  /**
   * A command without opening the session's socket (inbox answers). It goes under an id, as on the
   * socket: a message's own (its id is its key), else `key`, one made now unless given, taken once
   * however often it's sent under it (docs/wire.md, "The UI's sockets").
   */
  command: (id: string, c: Command, key: string = clientId()) =>
    call<void>("POST", `/api/harness/sessions/${encodeURIComponent(id)}/command`, c, undefined, c.type === "send" || c.type === "skill_run" ? undefined : { "idempotency-key": key }),
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

/**
 * A client-made id for a message or request (delivery state is reported under it): a UUIDv7, 48
 * bits of the time and 74 random, as 32 hex digits. A message's id is made once and every retry
 * carries it, and the agent's inbox takes each id once (CP-T3.2), so it mustn't meet another's.
 */
export const clientId = (): string => {
  const b = new Uint8Array(16);
  crypto.getRandomValues(b);
  const ms = Date.now();
  b[0] = Math.floor(ms / 2 ** 40) & 0xff;
  b[1] = Math.floor(ms / 2 ** 32) & 0xff;
  b[2] = (ms >>> 24) & 0xff;
  b[3] = (ms >>> 16) & 0xff;
  b[4] = (ms >>> 8) & 0xff;
  b[5] = ms & 0xff;
  b[6] = ((b[6] ?? 0) & 0x0f) | 0x70;
  b[8] = ((b[8] ?? 0) & 0x3f) | 0x80;
  return Array.from(b, (x) => x.toString(16).padStart(2, "0")).join("");
};

/* ------------------------------------------------------------------ live session */

export type Conn = "connecting" | "open" | "reconnecting" | "gone";

/** A command this page sent under an id it made (docs/wire.md, "The UI's sockets"): waiting for its answer (sent again on the next socket until it comes), or refused, saying how and why. */
export interface Asked {
  id: string;
  command: Command;
  /** When it was first sent (ms). */
  at: number;
  state: "pending" | "refused";
  code?: string;
  why?: string;
  /** How many times it was answered for now (parked, or to send again), and sent again for it. */
  tries?: number;
}

/** How long a command waits for an answer, sent again on each socket meanwhile: past it, it's said to have had none. One answered for now (parked, or to send again) has had one, and goes on. */
export const ASKED_FOR_MS = 10 * 60_000;
/** How long a refused command stays shown. */
export const REFUSED_FOR_MS = 15_000;
/** How long a command answered for now (parked, or a nack the server says to send `again`) waits before it goes again under its id: doubling from the first, to the last at most. */
export const AGAIN_AFTER_MS = [10_000, 20_000, 40_000, 60_000];

/** The id a command goes under: a message's own (its inbox takes it once under it), else one made now. */
export function commandId(c: Command): string {
  if (c.type === "send" && c.id) return c.id;
  if (c.type === "skill_run" && c.id) return c.id;
  return clientId();
}

export interface Snapshot {
  conn: Conn;
  info: AgentInfo | null;
  meta: Meta | null;
  state: State | null;
  /** In timeline order. */
  events: HEvent[];
  /** The changeset, file by file (null until the server sends one). */
  changes: FileChange[] | null;
  /** Files its finished messages mention, as the server kept them. */
  files: KeptFile[];
  error: string | null;
  /** The first snapshot is in: until then there's nothing to show but a loader. */
  ready: boolean;
  /** A stop asked of the turn under way: which turn, and when (ms). Done with once none runs. */
  stopAsked: { turn: number | undefined; at: number } | null;
  /** When the turn under way began, kept once it ends: between turns the clock holds the last one's length. */
  ranFrom: number | null;
  /** The commands this page sent that haven't been answered, and those refused a moment ago. */
  asked: Asked[];
  /** Messages sent from this page that the session doesn't show yet, in the order they went. */
  outgoing: Outgoing[];
}

/**
 * A message sent from this page, shown in the conversation at once until its session shows it
 * there or in a queue: on its way (`sending`, sent again under its id until it's answered:
 * `Snapshot.asked`), taken by its machine (`sent`), or refused for good (`failed`, `why`). Part of
 * CP-T3.4: nothing sent is out of sight while it goes.
 */
export interface Outgoing {
  id: string;
  text: string;
  /** Its files, by name. */
  attachments: string[];
  reply_to?: string;
  /** The subagent it's for (its task's id). */
  task?: string;
  mode?: SendMode;
  /** When it was sent (ms). */
  at: number;
  state: "sending" | "sent" | "failed";
  why?: string;
}

const ENDED = "the agent ended before it got there";

/** A message on its way from here, and what was sent (a retry sends it again, as a new message). */
interface Going {
  o: Outgoing;
  c: Extract<Command, { type: "send" }>;
}

/**
 * When the last turn began, for the clock between turns: as this page saw it start, else (opened
 * since, or the start went by unseen) when it was last asked something before it ended. Null while
 * a turn runs, and before one has ended.
 */
export function lastTurnFrom(s: Snapshot): number | null {
  const end = s.state?.last_turn?.at;
  if (end == null || s.state?.turn_started != null) return null;
  let asked = 0;
  for (let i = s.events.length - 1; i >= 0; i--) {
    const e = s.events[i];
    if (e?.kind === "user" && e.at <= end) {
      asked = e.at;
      break;
    }
  }
  const seen = s.ranFrom != null && s.ranFrom <= end ? s.ranFrom : 0;
  // What it saw start, unless a later ask began a turn it didn't see.
  return seen && seen + 2000 >= asked ? seen : asked || null;
}

/** A promise React reads as settled without waiting (its `status`, as React marks the ones it tracks). */
const READY: Promise<void> = Object.assign(Promise.resolve(), { status: "fulfilled", value: undefined });

const EMPTY: Snapshot = {
  conn: "connecting",
  info: null,
  meta: null,
  state: null,
  events: [],
  changes: null,
  files: [],
  error: null,
  ready: false,
  stopAsked: null,
  ranFrom: null,
  asked: [],
  outgoing: [],
};
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
/** Match the server mirror's rolling event window (harness/core.rs and registry.rs). */
const MAX_EVENTS = 4000;
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
  files: KeptFile[] | null;
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
  /** How far what's shown goes in the server's copy (docs/wire.md, "Revisions"): its process's
   * incarnation and the copy's revision. A reconnect asks for only what came after. */
  private born = 0;
  private rev = 0;
  /** What an "open" or "resumed" said, taken once what follows it is in. */
  private pending: { born: number; rev: number } | null = null;
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
  /** The commands sent and not answered yet, and those refused a moment ago, by id. */
  private asked = new Map<string, Asked>();
  private waiting: { promise: Promise<void>; resolve: () => void } | null = null;
  /** Packed messages being unpacked, and the chain they (and what came after them) are taken in. */
  private unpacking = 0;
  private inbox: Promise<void> = Promise.resolve();
  /** Messages sent from here that the session doesn't show yet, in the order they went (`Snapshot.outgoing`). */
  private out = new Map<string, Going>();

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

  /**
   * Send a command; false when the session isn't connected. It goes under an id ({@link commandId}),
   * taken once however often it's sent: kept until its answer comes, it's sent again on each socket
   * after this one, and shown as waiting meanwhile, or refused once it is (`Snapshot.asked`). A
   * message also shows in the conversation until the session has it (`Snapshot.outgoing`).
   */
  send = (c: Command): boolean => {
    if (this.ws?.readyState !== WebSocket.OPEN) return false;
    const id = commandId(c);
    this.asked.set(id, { id, command: c, at: Date.now(), state: "pending" });
    this.ws.send(JSON.stringify({ t: "cmd", id, c } satisfies SessionUp));
    this.patch({ asked: [...this.asked.values()] });
    if (c.type === "send") this.track(c);
    return true;
  };

  /**
   * Sends a message refused for good again, as a new message with its words, files and reply (its
   * id's answer stays the refusal): false while the session isn't connected.
   */
  resend = (id: string): boolean => {
    const m = this.out.get(id);
    if (m?.o.state !== "failed" || !this.send({ ...m.c, id: clientId() })) return false;
    this.out.delete(id);
    this.outChanged();
    return true;
  };

  /** Lets go of a message that didn't get there: it isn't sent again. */
  discard = (id: string) => {
    if (this.out.delete(id)) this.outChanged();
  };

  /** The socket's open: what was sent and not answered goes again under its id; what waited too long for any answer is said to have had none. */
  private sendAgain(ws: WebSocket) {
    const now = Date.now();
    let changed = false;
    for (const a of this.asked.values()) {
      if (a.state !== "pending") continue;
      if (!a.tries && now - a.at > ASKED_FOR_MS) {
        this.refuse(a.id, "unanswered", "no answer came");
        changed = true;
        continue;
      }
      ws.send(JSON.stringify({ t: "cmd", id: a.id, c: a.command } satisfies SessionUp));
    }
    if (changed) this.patch({ asked: [...this.asked.values()] });
  }

  /** Command `id` was answered for now: it waits, and goes again under its id in a while, on this socket if it's still open (or on the next as any waiting one does). */
  private later(id: string, why?: string) {
    const a = this.asked.get(id);
    if (!a || a.state !== "pending") return;
    const tries = (a.tries ?? 0) + 1;
    this.asked.set(id, { ...a, tries, why });
    this.patch({ asked: [...this.asked.values()] });
    const ws = this.ws;
    window.setTimeout(() => {
      const now = this.asked.get(id);
      if (ws && this.ws === ws && ws.readyState === WebSocket.OPEN && now?.state === "pending" && now.tries === tries) ws.send(JSON.stringify({ t: "cmd", id, c: now.command } satisfies SessionUp));
    }, AGAIN_AFTER_MS[Math.min(tries, AGAIN_AFTER_MS.length) - 1]);
  }

  /** Command `id` was refused: shown so a moment, then let go of. A message refused so says so where it shows in the conversation, until it's sent again or let go of. */
  private refuse(id: string, code: string, why: string) {
    const a = this.asked.get(id);
    if (!a) return;
    this.asked.set(id, { ...a, state: "refused", code, why });
    this.fail([id], why);
    window.setTimeout(() => {
      if (this.asked.get(id)?.state !== "refused") return;
      this.asked.delete(id);
      this.patch({ asked: [...this.asked.values()] });
    }, REFUSED_FOR_MS);
  }

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
      // What's shown already: only what came after it, when the server's copy can say.
      const after = this.born && this.rev ? `&after=${this.born}.${this.rev}` : "";
      ws = new WebSocket(wsUrl(`/api/harness/sessions/${encodeURIComponent(this.id)}/ws${TAKES}${after}`));
    } catch {
      this.retry();
      return;
    }
    ws.binaryType = "arraybuffer";
    this.ws = ws;
    this.wsAt = Date.now();
    this.unpacking = 0;
    ws.onopen = () => {
      if (this.ws === ws) this.sendAgain(ws);
    };
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
          if (this.ws === ws) this.unpacking -= 1;
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
    let u: SessionDown;
    try {
      u = JSON.parse(data) as SessionDown;
    } catch {
      return;
    }
    this.frames += 1;
    this.apply(u);
    this.mark(u);
  }

  /** Where a frame says the server's copy is: an "open" or "resumed" once what follows it is in, a
   * live frame at once. */
  private mark(u: SessionDown) {
    const rev = (u as { rev?: unknown }).rev;
    if (u.t === "open" || u.t === "resumed") {
      const born = (u as { born?: unknown }).born;
      this.pending = typeof born === "number" && typeof rev === "number" ? { born, rev } : null;
      return;
    }
    if (!this.incoming && typeof rev === "number" && rev > this.rev) this.rev = rev;
  }

  /** The socket closed: try again (rarely once it has ended, in case it was only away). */
  private lost() {
    if (!this.wanted) return;
    window.clearTimeout(this.quiet);
    window.clearTimeout(this.hold);
    // A snapshot or resume cut off isn't what's shown: a reconnect asks from what was before it.
    this.incoming = null;
    this.pending = null;
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

  private apply(u: SessionDown) {
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
      case "ack":
        // Parked for its machine, which is away: it waits, asked again in a while.
        if (u.outcome === "parked") return this.later(u.id, "its machine is away, and it goes once that's back");
        // A message its inbox hasn't answered yet: asked again, its inbox answers a repeat if it took it.
        if (u.outcome === "pending") return this.later(u.id, "its inbox hasn't said it took it yet");
        // Its machine has it: a message shows as sent until the conversation shows it.
        this.took(u.id);
        if (this.asked.delete(u.id)) this.patch({ asked: [...this.asked.values()] });
        return;
      case "nack":
        // Not this time (its machine away, its inbox full ...): it waits, and goes again.
        if (u.id != null && u.again) return this.later(u.id, u.why);
        if (u.id != null && this.asked.has(u.id)) {
          this.refuse(u.id, u.code, u.why);
          this.patch({ asked: [...this.asked.values()], error: u.why });
        } else this.patch({ error: u.why });
        return;
      case "open":
      case "resumed":
        this.opened = true;
        this.ended = false;
        this.misses = 0;
        this.attempt = 0;
        // Sent as the socket opens: the server's own snapshot, which it ends with "synced"; or
        // only what came after what's shown (resumed), taken in with it.
        this.begin(u.info, this.frames === 1, u.t === "resumed");
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
        this.fail(this.sending(), ENDED);
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

  private begin(info: AgentInfo | null, told: boolean, resumed = false) {
    window.clearTimeout(this.hold);
    // A resume adds to what's shown; a snapshot replaces it.
    const byId = resumed ? new Map(this.byId) : new Map<string, HEvent>();
    this.incoming = { info, meta: this.snap.meta, state: this.snap.state, changes: resumed ? this.snap.changes : null, files: null, byId, events: false, told };
    this.hold = window.setTimeout(() => this.commit(), HOLD_MS);
    this.wait();
  }

  private gather(inc: Incoming, u: SessionDown) {
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
      case "files":
        inc.files = withFiles(inc.files ?? [], u.files);
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
    // What's shown goes as far as the open or resume said.
    if (this.pending) {
      this.born = this.pending.born;
      this.rev = this.pending.rev;
      this.pending = null;
    }
    const s = this.snap;
    this.snap = {
      ...s,
      conn: "open",
      info: share(s.info, inc.info),
      meta: share(s.meta, inc.meta),
      state: share(s.state, inc.state),
      ranFrom: inc.state?.turn_started ?? s.ranFrom,
      changes: share(s.changes, inc.changes),
      files: inc.files ? share(s.files, inc.files) : s.files,
      error: null,
      ready: true,
    };
    // Ready only with its events in order, so the first render has them all.
    this.snap = { ...this.snap, events: this.ordered() };
    this.known = true;
    this.arrived(this.byId.values());
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
    this.fail(this.sending(), ENDED);
    this.settle();
  }

  private take(u: SessionDown) {
    switch (u.t) {
      case "changes":
        this.snap = { ...this.snap, changes: share(this.snap.changes, u.files) };
        break;
      case "files":
        this.snap = { ...this.snap, files: withFiles(this.snap.files, u.files) };
        break;
      case "state":
      case "state_patch": {
        const now = u.t === "state" ? u.state : patched(this.snap.state, u);
        const state = share<State | null>(this.snap.state, now) ?? now;
        // A stop asked of a turn holds while one runs; once none does, it's done with.
        const stopAsked = state.status === "running" || state.status === "waiting" ? this.snap.stopAsked : null;
        this.snap = { ...this.snap, state, info: withState(this.snap.info, state), stopAsked, ranFrom: state.turn_started ?? this.snap.ranFrom };
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
    if (u.t === "events") this.arrived(u.events);
    else if (u.t === "state" || u.t === "state_patch") this.arrived([]);
  }

  /* -------------------------------------------------------------- what's on its way from here */

  /** A message that went: shown as sending until the session has it (sent again, the same one, in its place and with its time). */
  private track(c: Extract<Command, { type: "send" }>) {
    if (this.shows(c.id)) return;
    const at = this.out.get(c.id)?.o.at ?? Date.now();
    const o: Outgoing = { id: c.id, text: c.text, attachments: (c.attachments ?? []).map((a) => a.name), reply_to: c.reply_to, task: c.task, mode: c.mode, at, state: "sending" };
    this.out.set(c.id, { o, c });
    this.outChanged();
  }

  /** Whether the session shows message `id`: in its conversation, or waiting in its queue or a subagent's. */
  private shows(id: string): boolean {
    const s = this.snap.state;
    if (s?.queue?.some((q) => q.id === id) || s?.tasks?.some((t) => t.queue?.some((q) => q.id === id))) return true;
    if (this.byId.get(id)?.kind === "user") return true;
    for (const e of this.byId.values()) if (e.kind === "user" && e.client_id === id) return true;
    return false;
  }

  /** Lets go of the messages sent from here that the session shows now: among `events` (those that came; all of them, after a snapshot), or in its queues. */
  private arrived(events: Iterable<HEvent>) {
    if (this.out.size === 0) return;
    const before = this.out.size;
    for (const e of events) {
      if (e.kind !== "user") continue;
      this.out.delete(e.id);
      if (e.client_id) this.out.delete(e.client_id);
    }
    const s = this.snap.state;
    for (const q of s?.queue ?? []) this.out.delete(q.id);
    for (const t of s?.tasks ?? []) for (const q of t.queue ?? []) this.out.delete(q.id);
    if (this.out.size !== before) this.outChanged();
  }

  /** The messages still on their way: not taken by their machine yet. */
  private sending(): string[] {
    return [...this.out.values()].filter((m) => m.o.state === "sending").map((m) => m.o.id);
  }

  /** Message `id`'s machine has it (its inbox took it, or its runner did): it shows as sent until the conversation shows it. */
  private took(id: string) {
    const m = this.out.get(id);
    if (m?.o.state !== "sending") return;
    this.out.set(id, { ...m, o: { ...m.o, state: "sent" } });
    this.outChanged();
  }

  /** Messages `ids` didn't get there, `why` (one its machine took did, whatever's said after). */
  private fail(ids: string[], why: string) {
    let changed = false;
    for (const id of ids) {
      const m = this.out.get(id);
      if (!m || m.o.state === "sent") continue;
      this.out.set(id, { ...m, o: { ...m.o, state: "failed", why } });
      changed = true;
    }
    if (changed) this.outChanged();
  }

  /** What's shown of the messages on their way changed. */
  private outChanged() {
    this.snap = { ...this.snap, outgoing: Array.from(this.out.values(), (m) => m.o) };
    this.schedule();
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
    let events = this.ordered();
    if (events.length > MAX_EVENTS + MAX_EVENTS / 4) {
      for (const e of events.slice(0, -MAX_EVENTS)) this.byId.delete(e.id);
      events = events.slice(-MAX_EVENTS);
      this.pos = new Map(events.map((e, i) => [e.id, i]));
    }
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
