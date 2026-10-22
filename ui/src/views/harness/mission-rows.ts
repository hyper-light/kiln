/**
 * Mission Control lists every agent kiln runs, one row each: an agent started through kiln's API is
 * a top-level agent, whoever started it. A harness's own subagents stay in their agent's chat.
 */
import type { AgentInfo, SaidHit } from "@/api/harness";
import type { AiSession } from "@/api/types";

export type MissionStand = "needs" | "crashed" | "paused" | "working" | "finished" | "ready" | "offline" | "ended";
export type MissionRow = { a: AgentInfo; stand: MissionStand };

/** Sessions kiln runs itself, and their subagents, aren't listed again as sessions found on the machines. */
export function missionFoundSessions(found: AiSession[], managed: { a: AgentInfo }[]): AiSession[] {
  const known = new Set(managed.flatMap(({ a }) => [a.harness_session, ...(a.subagents ?? []).map((task) => task.id)]).filter(Boolean));
  return found.filter((session) => !session.managed_by && !known.has(session.id));
}

/** A subagent's events share its agent's conversation, but aren't Mission Control search results. */
export function missionSaid(hits: SaidHit[], agents: ReadonlySet<string>): SaidHit[] {
  return hits.filter((hit) => agents.has(hit.agent) && !hit.event.parent);
}
