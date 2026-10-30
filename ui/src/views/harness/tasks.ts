/**
 * What runs beside an agent, as every view reads it: which tasks are its subagents, when they keep it
 * working, and who can stop each. An agent is at work while its turn runs and, its turn over, while a
 * subagent it sent off still works (it goes on when that one reports back); shells, monitors,
 * workflows and background calls it left running never keep it working by themselves. The server
 * says the same (`stand` in harness/wait.rs, `SubagentBrief::of`, `Task::live`).
 */
import type { AgentInfo, Approval, ApprovalBrief, SendMode, Status, SubagentBrief, Task } from "@/api/harness";
import { agentLabel, taskLive } from "./derive";

/**
 * The subagents kiln started for an agent's goals or plan steps side by side (`subagent`): sessions
 * of their own, on its machine and with its settings, that are its as its harness's own subagents
 * are, in its chat and on its row (docs/goals.md, "A plan's steps side by side"). Not agents of
 * their own: Mission Control lists none of them while their agent is there.
 */
export const kilnSubagentsOf = (list: readonly AgentInfo[], id: string): AgentInfo[] => list.filter((a) => a.subagent === true && a.parent === id);

/** Where one of them stands, in a task's words: running, paused (on you, or for a usage limit), pending (its machine away), else done. */
export function kilnStatus(a: Pick<AgentInfo, "status" | "approvals" | "pause" | "exit" | "held">): string {
  if (a.status === "running" || a.status === "starting") return "running";
  if (a.status === "waiting" || a.approvals.length > 0 || a.held || a.pause) return "paused";
  if (a.status === "disconnected") return "pending";
  if (a.status === "exited" && a.exit?.error) return "failed";
  return "completed";
}

/** Those of them at work: running, waiting on you or a limit, or their machine away (as `taskLive` has a task). */
export const kilnAtWork = (subs: readonly AgentInfo[]): AgentInfo[] => subs.filter((a) => kilnStatus(a) !== "completed" && kilnStatus(a) !== "failed");

/** One of them as lists show an agent's subagents (`SubagentBrief`: its id is its session's). */
export const kilnBrief = (a: AgentInfo): SubagentBrief => ({ id: a.id, description: agentLabel(a), status: kilnStatus(a), started: a.turn_started ?? a.created_at });

/** An agent as its row shows it: the subagents kiln started for it among its own. */
export const withKilnSubagents = (a: AgentInfo, subs: readonly AgentInfo[]): AgentInfo => (subs.length > 0 ? { ...a, subagents: [...(a.subagents ?? []), ...subs.map(kilnBrief)] } : a);

/** What a task is; machines from before `class` only say the harness's own task type. */
export const classOf = (t: Pick<Task, "class" | "kind">): string =>
  t.class ?? (t.kind?.startsWith("monitor") ? "monitor" : t.kind === "local_bash" ? "shell" : t.kind === "local_workflow" ? "workflow" : t.kind?.includes("mcp") ? "mcp" : "agent");

/** One of its own subagents, not something it left running in the background. */
export const isSubagent = (t: Pick<Task, "class" | "kind">): boolean => classOf(t) === "agent";

/** What a request asks, in one line, as Mission control's row says it. */
export const askWords = (ap: Pick<ApprovalBrief, "tool" | "tool_name" | "title">): string =>
  ap.tool === "question" ? `Asks: ${ap.title}` : ap.tool === "plan" ? "Proposes a plan" : `${ap.tool_name || ap.tool}: ${ap.title}`;

/**
 * One of its subagents waiting on you: its first request, its name, the session the answer goes to
 * (`who`: a subagent kiln started is a session of its own; one of its harness's own asks through its
 * agent's), and what opens its thread (`thread`, as the agent page's `onOpenTask` takes it).
 */
export interface SubagentAsk {
  ap: Pick<ApprovalBrief, "id" | "tool" | "tool_name" | "title" | "at">;
  name: string;
  who: string;
  thread: string;
}

/**
 * The subagents of agent `agent` that wait on you, longest waiting first: those kiln started for it
 * with a request (`kiln`), and its harness's own, whose requests are the agent's and name them
 * (`Approval.agent`: Claude Code by the task's id, Codex by what it was sent to do). What you
 * answered here (`done`, answered.ts) no longer waits.
 */
export function subagentAsks(
  agent: string,
  approvals: readonly Pick<Approval, "id" | "tool" | "tool_name" | "title" | "at" | "agent">[],
  tasks: readonly Task[],
  kiln: readonly AgentInfo[],
  done: ReadonlyMap<string, number>,
): SubagentAsk[] {
  const asks: SubagentAsk[] = [];
  for (const a of kiln) {
    const ap = a.approvals.find((p) => !done.has(p.id));
    if (ap) asks.push({ ap, name: agentLabel(a), who: a.id, thread: a.id });
  }
  const asking = new Set<string>();
  for (const ap of approvals) {
    if (!ap.agent || done.has(ap.id)) continue;
    const t = tasks.find((x) => isSubagent(x) && (x.id === ap.agent || x.tool_id === ap.agent || x.description === ap.agent));
    if (!t || asking.has(t.id)) continue;
    asking.add(t.id);
    asks.push({ ap, name: t.description || ap.agent, who: agent, thread: t.tool_id ?? t.id });
  }
  return asks.sort((x, y) => x.ap.at - y.ap.at);
}

/** Its subagents still at work (running, waiting to start, or paused for an approval), as the agents list says. */
export const subagentsAtWork = (subagents: readonly SubagentBrief[] | undefined): SubagentBrief[] => (subagents ?? []).filter(taskLive);

/** The same from its live state, whose tasks hold its shells and monitors too. */
export const subagentTasksAtWork = (tasks: readonly Task[] | undefined): Task[] => (tasks ?? []).filter((t) => isSubagent(t) && taskLive(t));

/** Its turn is over, but `atWork` subagents it sent off still work: it stands as working. */
export const workingThrough = (status: Status, atWork: number): boolean => status === "idle" && atWork > 0;

/** {@link workingThrough}, from the agents list. */
export const throughSubagents = (a: Pick<AgentInfo, "status" | "subagents">): boolean => workingThrough(a.status, subagentsAtWork(a.subagents).length);

/** A subagent running right now: what the holographic shimmer marks (one waiting to start or paused for an approval holds still). */
export const subagentRunning = (subagents: readonly Pick<Task, "status">[] | undefined): boolean => (subagents ?? []).some((t) => t.status === "running");

/** What kiln took over when an agent's harness stopped (its own `left-<pid>` tasks): kiln stops it, whatever the harness. */
export const LEFT = "left-";

/** A subagent that takes a person's messages now (`takes`): over its thread, the composer writes to it, not the agent. */
export const takesMessages = (t: Pick<Task, "takes"> | undefined): boolean => (t?.takes?.length ?? 0) > 0;

/** The field's words over a subagent's thread: how what's written reaches it (`mode`: how, while it works; `viaParent`: its agent passes it on). */
export function subagentPlaceholder(mode: SendMode | null, viaParent: boolean): string {
  const how =
    mode === "steer"
      ? "Steer the subagent — it reads this at its next step"
      : mode === "now"
        ? "Stop the subagent and say…"
        : mode === "queue"
          ? "Queue a message for when the subagent finishes"
          : "Message the subagent";
  return viaParent ? `${how} (the main agent passes it on)` : how;
}

/**
 * Who can stop a task: kiln itself (what it took over), its harness (the task says so; a server
 * from before `stoppable` only says whether the harness stops tasks at all), or no one yet.
 */
export function stopperOf(t: Pick<Task, "id" | "stoppable">, features: readonly string[]): "kiln" | "harness" | null {
  if (t.id.startsWith(LEFT)) return "kiln";
  return (t.stoppable ?? features.includes("stop_task")) ? "harness" : null;
}
