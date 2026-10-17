/**
 * Views over an agent's timeline: turns made of steps (not one row per tool call), the cumulative
 * changeset, the commands it ran. Pure functions of the event list, memoized by the caller.
 */
import type { FileDiff, HEvent, ToolCall, ToolKind } from "@/api/harness";

export type ToolEvent = Extract<HEvent, { kind: "tool" }>;

/** A run of related tool calls shown as one line ("Read 6 files", "Edited 3 files +42 −7"). */
export interface Step {
  type: "step";
  key: string;
  group: StepGroup;
  calls: ToolEvent[];
}

export type StepGroup = "explore" | "edit" | "shell" | "web" | "agent" | "plan" | "ask" | "tool";

export type Item =
  | Step
  | { type: "note"; key: string; event: Extract<HEvent, { kind: "assistant" }> }
  | { type: "thought"; key: string; event: Extract<HEvent, { kind: "thinking" }> }
  | { type: "notice"; key: string; event: Extract<HEvent, { kind: "notice" }> };

export interface Turn {
  key: string;
  /** What the person asked (null for what happened before the first message). */
  prompt: Extract<HEvent, { kind: "user" }> | null;
  items: Item[];
  /** The agent's last words in the turn: its answer. */
  outcome: Extract<HEvent, { kind: "assistant" }> | null;
  started: number;
  ended: number | null;
}

const GROUP: Record<ToolKind, StepGroup> = {
  read: "explore",
  search: "explore",
  edit: "edit",
  shell: "shell",
  web: "web",
  agent: "agent",
  todo: "plan",
  plan: "plan",
  question: "ask",
  skill: "tool",
  mcp: "tool",
  other: "tool",
};

/** Kinds where consecutive calls read as one step. */
const MERGES: Partial<Record<StepGroup, boolean>> = { explore: true, edit: true, shell: true, web: true, tool: true, plan: true };

/** The main thread's timeline as turns of steps. Subagent events (`parent` set) stay with their task. */
export function turnsOf(events: HEvent[], parent?: string): Turn[] {
  const turns: Turn[] = [];
  let cur: Turn | null = null;
  const open = (prompt: Turn["prompt"], at: number, key: string) => {
    cur = { key, prompt, items: [], outcome: null, started: at, ended: null };
    turns.push(cur);
    return cur;
  };
  for (const e of events) {
    if ((e.parent ?? undefined) !== parent) continue;
    if (e.kind === "user") {
      if (cur) (cur as Turn).ended = e.at;
      open(e, e.at, e.id);
      continue;
    }
    const t: Turn = cur ?? open(null, e.at, `start-${e.id}`);
    t.ended = e.at;
    if (e.kind === "tool") {
      const g = GROUP[e.tool] ?? "tool";
      const last = t.items.at(-1);
      if (last && last.type === "step" && last.group === g && MERGES[g]) last.calls.push(e);
      else t.items.push({ type: "step", key: e.id, group: g, calls: [e] });
    } else if (e.kind === "assistant") {
      t.items.push({ type: "note", key: e.id, event: e });
    } else if (e.kind === "thinking") {
      t.items.push({ type: "thought", key: e.id, event: e });
    } else {
      t.items.push({ type: "notice", key: e.id, event: e });
    }
  }
  // The last assistant note of a turn, if nothing but thoughts follow it, is the turn's answer.
  for (const t of turns) {
    for (let i = t.items.length - 1; i >= 0; i--) {
      const it = t.items[i];
      if (!it || it.type === "thought" || it.type === "notice") continue;
      if (it.type === "note" && !it.event.streaming) {
        t.outcome = it.event;
        t.items.splice(i, 1);
      }
      break;
    }
  }
  return turns;
}

export function diffStats(diffs: FileDiff[] | undefined): { add: number; del: number } {
  let add = 0;
  let del = 0;
  for (const d of diffs ?? [])
    for (const h of d.hunks)
      for (const l of h.lines) {
        if (l.startsWith("+")) add++;
        else if (l.startsWith("-")) del++;
      }
  return { add, del };
}

export interface FileChange {
  path: string;
  created: boolean;
  deleted?: boolean;
  truncated?: boolean;
  add: number;
  del: number;
  /** Every edit to the file, in order, with the call that made it (one cumulative diff, call null, from the server). */
  edits: { call: ToolEvent | null; diff: FileDiff }[];
}

/** Everything the agent changed (or proposed and was refused), file by file. */
export function changesetOf(events: HEvent[]): FileChange[] {
  const files = new Map<string, FileChange>();
  for (const e of events) {
    if (e.kind !== "tool" || e.tool !== "edit" || e.status === "denied") continue;
    for (const d of e.diffs ?? []) {
      let f = files.get(d.path);
      if (!f) {
        f = { path: d.path, created: Boolean(d.created), add: 0, del: 0, edits: [] };
        files.set(d.path, f);
      }
      const s = diffStats([d]);
      f.add += s.add;
      f.del += s.del;
      f.edits.push({ call: e, diff: d });
    }
  }
  return [...files.values()].sort((a, b) => a.path.localeCompare(b.path));
}

export function runsOf(events: HEvent[]): ToolEvent[] {
  return events.filter((e): e is ToolEvent => e.kind === "tool" && e.tool === "shell");
}

/** What a group of calls did, in a few words. */
export function stepSummary(group: StepGroup, calls: ToolCall[]): string {
  const n = calls.length;
  const plural = (k: number, one: string, many = `${one}s`) => `${k} ${k === 1 ? one : many}`;
  switch (group) {
    case "explore": {
      const reads = calls.filter((c) => c.tool === "read").length;
      const searches = n - reads;
      const parts = [];
      if (reads) parts.push(`read ${plural(reads, "file")}`);
      if (searches) parts.push(`searched ${plural(searches, "time")}`);
      const s = parts.join(", ");
      return s.charAt(0).toUpperCase() + s.slice(1);
    }
    case "edit": {
      const files = new Set(calls.flatMap((c) => (c.diffs ?? []).map((d) => d.path)));
      return `Edited ${plural(files.size || n, "file")}`;
    }
    case "shell":
      return n === 1 ? "Ran a command" : `Ran ${n} commands`;
    case "web":
      return n === 1 ? "Looked something up" : `Looked up ${n} things`;
    case "agent":
      return "Started a subagent";
    case "plan":
      return calls.some((c) => c.tool === "plan") ? "Proposed a plan" : "Updated the plan";
    case "ask":
      return "Asked you";
    default: {
      const servers = [...new Set(calls.map((c) => c.server).filter(Boolean))];
      const skills = [...new Set(calls.map((c) => c.skill).filter(Boolean))];
      if (skills.length && skills.length === calls.length) return skills.length === 1 ? `Used the ${skills[0]} skill` : `Used ${skills.length} skills`;
      if (servers.length === 1 && calls.every((c) => c.server)) return n === 1 ? `Used ${servers[0]}` : `Used ${servers[0]} ${n} times`;
      return n === 1 ? `Used ${calls[0]?.tool_short ?? calls[0]?.name ?? "a tool"}` : `Used ${n} tools`;
    }
  }
}

/** The worst status among calls (for the step's dot). */
export function stepStatus(calls: ToolCall[]): ToolCall["status"] {
  const order: ToolCall["status"][] = ["error", "denied", "pending", "running", "cancelled", "done"];
  for (const s of order) if (calls.some((c) => c.status === s)) return s;
  return "done";
}

export function tildify(path: string): string {
  return path.replace(/^\/(?:home|Users)\/[^/]+(?=\/|$)/, "~");
}

/** A path relative to the agent's folder when it is inside it. */
export function relPath(path: string, cwd: string): string {
  if (cwd && path.startsWith(`${cwd}/`)) return path.slice(cwd.length + 1);
  return tildify(path);
}

export function duration(ms: number): string {
  if (ms < 1000) return `${Math.max(0, Math.round(ms))}ms`;
  const s = ms / 1000;
  if (s < 60) return `${s < 10 ? s.toFixed(1) : Math.round(s)}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ${Math.round(s % 60)}s`;
  return `${Math.floor(m / 60)}h ${m % 60}m`;
}

export function money(usd: number): string {
  if (!usd) return "$0";
  return usd < 0.01 ? "<$0.01" : usd < 10 ? `$${usd.toFixed(2)}` : `$${Math.round(usd)}`;
}

/**
 * What a harness's cost figure is: Claude Code and Codex report their own estimate at API list
 * prices. On a subscription nobody is billed that; on an API key it's an estimate of the bill.
 */
export function costText(usd: number, plan?: string | null): { text: string; title: string } {
  const m = money(usd);
  const p = (plan ?? "").toLowerCase();
  if (/api|bedrock|vertex|foundry|azure/.test(p)) return { text: `≈${m} est.`, title: "The agent's own estimate of the cost at API list prices." };
  if (p && !/signed out|unknown/.test(p))
    return { text: `≈${m} API value`, title: `The agent's estimate at API list prices. Your ${plan} plan isn't billed per token; its limits are under Tools → Usage.` };
  return { text: `≈${m}`, title: "The agent's own estimate at API list prices." };
}

/** "claude-opus-5-5[1m]" → "Opus 5.5 (1M)"; anything else is shown as it is. */
export function modelLabel(id: string): string {
  const m = id.match(/^claude-(opus|sonnet|haiku|fable)-(\d+)-(\d+)(?:-\d{8})?(\[1m\])?$/i);
  if (m) return `${m[1]!.charAt(0).toUpperCase()}${m[1]!.slice(1).toLowerCase()} ${m[2]}.${m[3]}${m[4] ? " (1M)" : ""}`;
  const alias = id.match(/^(opus|sonnet|haiku|fable|default)(\[1m\])?$/i);
  if (alias) return `${alias[1]!.charAt(0).toUpperCase()}${alias[1]!.slice(1).toLowerCase()}${alias[2] ? " (1M)" : ""}`;
  return id;
}

export function tokens(n: number | undefined): string {
  if (!n) return "0";
  return n < 1000 ? String(n) : n < 1_000_000 ? `${(n / 1000).toFixed(n < 10_000 ? 1 : 0)}k` : `${(n / 1_000_000).toFixed(1)}M`;
}

/** Markdown as one line of plain text (card previews). */
export function plain(md: string): string {
  return md
    .replace(/```[\s\S]*?```/g, " ")
    .replace(/`([^`]*)`/g, "$1")
    .replace(/\*\*([^*]+)\*\*/g, "$1")
    .replace(/(^|\s)[*_]([^*_\s][^*_]*)[*_]/g, "$1$2")
    .replace(/^\s*(#{1,6}|[-*]|\d+[.)])\s+/gm, "")
    .replace(/\[([^\]]+)\]\([^)]*\)/g, "$1")
    .replace(/\s+/g, " ")
    .trim();
}
