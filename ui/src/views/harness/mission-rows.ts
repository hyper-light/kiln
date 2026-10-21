/** Mission Control lists standalone agents; their children stay in the agent chats. */
import type { AgentInfo, SaidHit } from "@/api/harness";
import type { AiSession } from "@/api/types";

export type MissionStand = "needs" | "crashed" | "paused" | "working" | "finished" | "ready" | "offline" | "ended";
export type MissionRow = { a: AgentInfo; stand: MissionStand };

/** Apply before counts, filters and actions. A missing parent session doesn't make a child standalone. */
export function missionRows(managed: MissionRow[]): MissionRow[] {
  return managed.filter(({ a }) => !a.parent);
}

/** Hidden children must not reappear as discovered sessions. Use the full, unfiltered roster. */
export function missionFoundSessions(found: AiSession[], managed: { a: AgentInfo }[]): AiSession[] {
  const known = new Set(managed.flatMap(({ a }) => [a.harness_session, ...(a.subagents ?? []).map((task) => task.id)]).filter(Boolean));
  return found.filter((session) => !session.managed_by && !known.has(session.id));
}

/** Native child events share the owner's conversation, but aren't Mission Control search results. */
export function missionSaid(hits: SaidHit[], agents: ReadonlySet<string>): SaidHit[] {
  return hits.filter((hit) => agents.has(hit.agent) && !hit.event.parent);
}
