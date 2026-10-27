/**
 * Mission Control lists every agent kiln runs, one row each: an agent started through kiln's API is
 * a top-level agent, whoever started it. A harness's own subagents stay in their agent's chat, and so
 * do the subagents kiln starts for an agent's goals or plan steps side by side (`subagent`): they're
 * on their agent's row, as its harness's own are.
 */
import type { AgentInfo, SaidHit, SubagentBrief } from "@/api/harness";
import type { AiSession } from "@/api/types";
import { kilnSubagentsOf, throughSubagents, withKilnSubagents } from "./tasks";

export type MissionStand = "needs" | "crashed" | "paused" | "working" | "finished" | "ready" | "offline" | "ended";
/** An agent's row; `asking`: one of the subagents kiln started for it, whose request for you is on its row. */
export type MissionRow = { a: AgentInfo; stand: MissionStand; asking?: AgentInfo };

/** The agents that are rows: all but the subagents kiln started for an agent that's listed (one whose agent has gone is a row of its own, so nothing goes unseen). */
export function missionAgents(list: readonly AgentInfo[]): AgentInfo[] {
  const ids = new Set(list.map((a) => a.id));
  return list.filter((a) => !(a.subagent && a.parent && ids.has(a.parent)));
}

/**
 * Each row as it stands (`seen`: when you last looked at each agent): its agent with the subagents
 * kiln started for it among its own, so it works through them as through its harness's own; and,
 * while it doesn't need you itself, the first of them that does, whose request is then its row's.
 */
export function missionRows(list: readonly AgentInfo[], seen: Readonly<Record<string, number>>): MissionRow[] {
  return missionAgents(list).map((agent) => {
    const subs = kilnSubagentsOf(list, agent.id);
    const a = withKilnSubagents(agent, subs);
    const stand = standOf(a, seen[a.id]);
    const asking = stand === "needs" ? undefined : subs.find((s) => standOf(s, undefined) === "needs");
    return asking ? { a, stand: "needs", asking } : { a, stand };
  });
}

/** Where an agent stands (`seen`: when you last looked at it). Its turn over, a subagent it sent off still at work keeps it working (tasks.ts). */
export function standOf(a: AgentInfo, seen: number | undefined): MissionStand {
  // A request, or goals it paused for you.
  if (a.approvals.length > 0 || a.status === "waiting" || a.held) return "needs";
  if (a.status === "exited" && a.exit?.error) return "crashed";
  // Waiting for a usage limit to reset (its current turn, if any, finishes first).
  if (a.pause && a.status !== "running" && a.status !== "exited" && a.status !== "disconnected") return "paused";
  if (a.status === "running" || a.status === "starting" || throughSubagents(a)) return "working";
  if (a.status === "idle") return a.last_activity > (seen ?? 0) + 1500 ? "finished" : "ready";
  // Its machine is reconnecting (a runner or server restart): it stays where it is.
  if (a.status === "disconnected") return "offline";
  return "ended";
}

/** What an agent working through its subagents is doing: how many, and what each does. */
export const subagentsLine = (subs: readonly SubagentBrief[]): string => [subs.length === 1 ? "1 subagent" : `${subs.length} subagents`, ...subs.map((s) => s.description)].join(" · ");

/** Sessions kiln runs itself, and their subagents, aren't listed again as sessions found on the machines. */
export function missionFoundSessions(found: AiSession[], managed: { a: AgentInfo }[]): AiSession[] {
  const known = new Set(managed.flatMap(({ a }) => [a.harness_session, ...(a.subagents ?? []).map((task) => task.id)]).filter(Boolean));
  return found.filter((session) => !session.managed_by && !known.has(session.id));
}

/** A subagent's events share its agent's conversation, but aren't Mission Control search results. */
export function missionSaid(hits: SaidHit[], agents: ReadonlySet<string>): SaidHit[] {
  return hits.filter((hit) => agents.has(hit.agent) && !hit.event.parent);
}
