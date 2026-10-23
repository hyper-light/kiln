/**
 * What runs beside an agent, as every view reads it: which tasks are its subagents, when they keep it
 * working, and who can stop each. An agent is at work while its turn runs and, its turn over, while a
 * subagent it sent off still works (it goes on when that one reports back); shells, monitors,
 * workflows and background calls it left running never keep it working by themselves. The server
 * says the same (`stand` in harness/wait.rs, `SubagentBrief::of`, `Task::live`).
 */
import type { AgentInfo, Status, SubagentBrief, Task } from "@/api/harness";
import { taskLive } from "./derive";

/** What a task is; machines from before `class` only say the harness's own task type. */
export const classOf = (t: Pick<Task, "class" | "kind">): string =>
  t.class ?? (t.kind?.startsWith("monitor") ? "monitor" : t.kind === "local_bash" ? "shell" : t.kind === "local_workflow" ? "workflow" : t.kind?.includes("mcp") ? "mcp" : "agent");

/** One of its own subagents, not something it left running in the background. */
export const isSubagent = (t: Pick<Task, "class" | "kind">): boolean => classOf(t) === "agent";

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

/**
 * Who can stop a task: kiln itself (what it took over), its harness (the task says so; a server
 * from before `stoppable` only says whether the harness stops tasks at all), or no one yet.
 */
export function stopperOf(t: Pick<Task, "id" | "stoppable">, features: readonly string[]): "kiln" | "harness" | null {
  if (t.id.startsWith(LEFT)) return "kiln";
  return (t.stoppable ?? features.includes("stop_task")) ? "harness" : null;
}
