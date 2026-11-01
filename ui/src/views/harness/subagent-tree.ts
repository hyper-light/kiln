/**
 * An agent's subagents as one tree, wherever it's drawn (under its row in Mission control, by the
 * composer): the subagents kiln started for it (sessions of their own whose `parent` it is, and
 * theirs in turn) and its harness's own (`SubagentBrief`, nested by `parent_task`), in the order
 * they started. Each says where it stands, what it's doing or said it did, what it asks, and where
 * its thread opens; each row has its piece of the path that draws the tree: the lines passing by
 * it (an ancestor's, on to that one's next), the line or curve into its mark, and the line on from
 * it. While it needs you, its own piece is tan.
 */
import type { AgentInfo, ApprovalBrief, SubagentBrief, Task } from "@/api/harness";
import { agentLabel, plain } from "./derive";
import { standOf } from "./mission-rows";
import { askWords, isSubagent, kilnSubagentsOf } from "./tasks";

/** Its machine, as Mission control's rows name it (parts.tsx's `machineName`; that module brings the page's styles with it). */
const machineName = (target: string) => (target === "local" ? "server" : target.replace(/^agent:/, ""));

/** Where a subagent stands, as its row says it. */
export type NodeStand = "working" | "needs" | "paused" | "done" | "failed" | "stopped" | "offline";

export const NODE_WORD: Record<NodeStand, string> = {
  working: "Working",
  needs: "Needs you",
  paused: "Paused",
  done: "Done",
  failed: "Failed",
  stopped: "Stopped",
  offline: "Offline",
};

/** One subagent, as the tree draws it. */
export interface SubagentNode {
  /** Unique in the tree: a kiln subagent's session id; a harness subagent's session and task. */
  key: string;
  /** A session of its own (kiln's), or its harness's own subagent: a task of `session`. */
  kind: "kiln" | "harness";
  /** The session it is (kiln's), or whose task it is: where what's sent to it, and its answers, go. */
  session: string;
  /** Its task's id in `session` (its harness's own). */
  task?: string;
  /** Its parent's key; null under the agent itself. */
  parent: string | null;
  depth: number;
  name: string;
  stand: NodeStand;
  /** What it's doing now, what it asks, or what it said it did, in a line. */
  line: string;
  /** Its line is a request's command or words (drawn in the mono face, as Mission control's are). */
  mono: boolean;
  /** The request it waits on. */
  ask?: ApprovalBrief;
  machine: string;
  harness: string;
  started: number;
  ended?: number;
  /** Where its thread opens: its agent's page, it shown there. */
  href: string;
}

/** A row's piece of the path along one lane: from its top or its mark, to its mark or its bottom. */
export interface Piece {
  lane: number;
  from: "top" | "mark";
  to: "mark" | "bottom";
  hot: boolean;
}

/** A row's piece of the tree: its lanes' lines, the curve into its mark from its parent's lane (a first child's), its mark's lane. */
export interface RowPath {
  lane: number;
  pieces: Piece[];
  curve: { from: number; to: number; hot: boolean } | null;
}

export type SubagentRow = SubagentNode & { path: RowPath };

/**
 * A harness's own subagent: as the agents list has it (what it's doing, never what it reported:
 * `SubagentBrief::of`), or from its agent's live state, which also has its harness's summary of it.
 */
export type Brief = SubagentBrief & { summary?: string };

/** A harness's own subagents from a live state's tasks, as the list has them and with their summaries. */
export function briefsOf(tasks: readonly Task[]): Brief[] {
  return tasks.filter(isSubagent).map((t) => ({
    id: t.id,
    tool_id: t.tool_id,
    description: t.description,
    status: t.status,
    parent_task: t.parent_task,
    started: t.started,
    ended: t.ended,
    last_tool: t.last_tool,
    summary: t.summary,
  }));
}

/** Its harness's own subagent's request: one that names it (by its task, its spawning call or what it was sent to do). */
const askOf = (approvals: readonly ApprovalBrief[], b: Brief): ApprovalBrief | undefined =>
  approvals.find((p) => p.agent != null && (p.agent === b.id || p.agent === b.tool_id || p.agent === b.description));

function harnessStand(b: Brief, ask: ApprovalBrief | undefined): NodeStand {
  if (ask) return "needs";
  switch (b.status) {
    case "completed":
      return "done";
    case "failed":
      return "failed";
    case "stopped":
    case "killed":
      return "stopped";
    default:
      // Running, waiting to start, or paused for a request already answered.
      return "working";
  }
}

function kilnStand(a: AgentInfo): NodeStand {
  switch (standOf(a, undefined)) {
    case "needs":
      return "needs";
    case "crashed":
      return a.exit?.error ? "failed" : "stopped";
    case "paused":
      return "paused";
    case "working":
      return "working";
    case "offline":
      return "offline";
    default:
      return "done";
  }
}

function kilnLine(a: AgentInfo, stand: NodeStand): string {
  const ap = a.approvals[0];
  if (stand === "needs") return ap ? askWords(ap) : "Waiting for you";
  if (stand === "done") return a.summary ? plain(a.summary) : a.goal || a.title || "";
  if (stand === "failed" || stand === "stopped") return a.exit?.error ?? "";
  if (stand === "offline") return "Its machine is offline";
  return a.doing || a.activity || "";
}

/** Where a subagent's thread opens: its agent's page, it shown there (`own`: a kiln subagent's own page, not redirected into its agent's). */
export function threadHref(root: string, owner: string, id: string): string {
  return owner === root ? `/agents/${root}?task=${encodeURIComponent(id)}` : `/agents/${owner}?own=1&task=${encodeURIComponent(id)}`;
}

/**
 * The subagents of `root`, in tree order: under each, those it started, kiln's and its harness's
 * own together in the order they started. `list` is every agent kiln runs (its kiln subagents and
 * theirs are found there); `root` may carry its live state's subagents and requests (`briefsOf`).
 */
export function subagentTree(root: AgentInfo, list: readonly AgentInfo[]): SubagentNode[] {
  const out: SubagentNode[] = [];
  const seen = new Set<string>();
  const walk = (owner: AgentInfo, parent: string | null, depth: number) => {
    const briefs: Brief[] = (owner.subagents ?? []).filter((b) => !list.some((a) => a.id === b.id && a.subagent === true));
    const ids = new Set(briefs.map((b) => b.id));
    const harness = (under: string | undefined) => briefs.filter((b) => (under ? b.parent_task === under : !b.parent_task || !ids.has(b.parent_task)));
    const place = (b: Brief, parentKey: string | null, d: number) => {
      const key = `${owner.id}/${b.id}`;
      if (seen.has(key)) return;
      seen.add(key);
      const ask = askOf(owner.approvals, b);
      const stand = harnessStand(b, ask);
      // What it's at: its harness's summary of it where the live state has one (Claude Code's progress, or what it reported), else its last tool.
      const line = ask ? askWords(ask) : stand === "done" ? (b.summary ?? "") : b.summary || b.last_tool || "";
      out.push({
        key,
        kind: "harness",
        session: owner.id,
        task: b.id,
        parent: parentKey,
        depth: d,
        name: b.description || "Subagent",
        stand,
        line,
        mono: !!ask,
        ask,
        machine: machineName(owner.target),
        harness: owner.kind_name,
        started: b.started,
        ended: b.ended,
        href: threadHref(root.id, owner.id, b.id),
      });
      for (const c of harness(b.id).sort((x, y) => x.started - y.started)) place(c, key, d + 1);
    };
    const kiln = kilnSubagentsOf(list, owner.id);
    const items = [
      ...harness(undefined).map((b) => ({ at: b.started, go: () => place(b, parent, depth) })),
      ...kiln.map((a) => ({
        at: a.created_at,
        go: () => {
          if (seen.has(a.id) || a.id === root.id) return;
          seen.add(a.id);
          const stand = kilnStand(a);
          const line = kilnLine(a, stand);
          out.push({
            key: a.id,
            kind: "kiln",
            session: a.id,
            parent,
            depth,
            name: agentLabel(a),
            stand,
            line,
            mono: stand === "needs",
            ask: a.approvals[0],
            machine: machineName(a.target),
            harness: a.kind_name,
            started: a.turn_started ?? a.created_at,
            href: threadHref(root.id, owner.id, a.id),
          });
          walk(a, a.id, depth + 1);
        },
      })),
    ].sort((x, y) => x.at - y.at);
    for (const it of items) it.go();
  };
  walk(root, null, 0);
  return out;
}

/** Each row's piece of the path, drawn from its place among the rows (`nodes`, in tree order). */
export function treePaths(nodes: readonly SubagentNode[]): RowPath[] {
  const byKey = new Map(nodes.map((n) => [n.key, n]));
  const kids = new Map<string | null, SubagentNode[]>();
  for (const n of nodes) kids.set(n.parent, [...(kids.get(n.parent) ?? []), n]);
  const sibling = (n: SubagentNode, by: number): SubagentNode | undefined => {
    const s = kids.get(n.parent) ?? [];
    return s[s.indexOf(n) + by];
  };
  return nodes.map((n) => {
    const hot = n.stand === "needs";
    const pieces: Piece[] = [];
    // Its ancestors' lanes pass by it on to their next ones.
    const line: SubagentNode[] = [];
    for (let p = n.parent; p != null; p = byKey.get(p)?.parent ?? null) {
      const a = byKey.get(p);
      if (!a) break;
      line.unshift(a);
    }
    line.forEach((a, k) => {
      if (sibling(a, 1)) pieces.push({ lane: k, from: "top", to: "bottom", hot: false });
    });
    const d = line.length;
    // Into its mark: a first child's curve from its parent's lane, else the line down its own.
    let curve: RowPath["curve"] = null;
    if (d > 0 && !sibling(n, -1)) curve = { from: d - 1, to: d, hot };
    else pieces.push({ lane: d, from: "top", to: "mark", hot });
    // On from it: to its next, or to its own first.
    if (sibling(n, 1) || (kids.get(n.key)?.length ?? 0) > 0) pieces.push({ lane: d, from: "mark", to: "bottom", hot });
    return { lane: d, pieces, curve };
  });
}

/** The tree's rows: each subagent with its piece of the path. */
export function subagentRows(root: AgentInfo, list: readonly AgentInfo[]): SubagentRow[] {
  const nodes = subagentTree(root, list);
  const paths = treePaths(nodes);
  return nodes.map((n, i) => ({ ...n, path: paths[i] ?? { lane: n.depth, pieces: [], curve: null } }));
}
