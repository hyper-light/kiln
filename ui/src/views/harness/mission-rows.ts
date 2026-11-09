/**
 * Mission Control lists every agent kiln runs, one row each: an agent started through kiln's API is
 * a top-level agent, whoever started it. A harness's own subagents stay in their agent's chat, and so
 * do the subagents kiln starts for an agent's goals or plan steps side by side (`subagent`): they're
 * on their agent's row, as its harness's own are.
 */
import type { AgentInfo, SaidHit, SubagentBrief } from "@/api/harness";
import type { AiSession } from "@/api/types";
import { withKilnSubagents } from "./tasks";
import { chatStand, type ChatStand } from "./chat-state";
import { requestsFor, type AttentionRequestSet } from "@/lib/attention-requests";
import type { AttentionUrgency } from "@/api/attention";

/** `archived`: a chat you archived, listed under the State filter's Archived (plan 01a199e1b177752e84dad5dc990e6a15). */
export type MissionStand = ChatStand | "archived";
/** An agent's row; `asking`: one of the subagents kiln started for it, whose request for you is on its row. */
export type MissionRow = {
  a: AgentInfo;
  stand: MissionStand;
  asking?: AgentInfo;
  updated?: boolean;
  rank?: number;
  pinned?: number;
  /** What holds it where it is: why it's placed so, or where the learned order put it. */
  reason?: string;
  /** You brought it back from a park kiln made by itself. */
  correction?: boolean;
  requestRank?: number;
  stale?: boolean;
  urgency?: AttentionUrgency;
  requests?: AttentionRequestSet;
};

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
  const children = new Map<string, AgentInfo[]>();
  for (const a of list) if (a.subagent && a.parent) children.set(a.parent, [...(children.get(a.parent) ?? []), a]);
  return missionAgents(list).map((agent) => {
    const subs: AgentInfo[] = [];
    const seenChildren = new Set([agent.id]);
    const queue = [...(children.get(agent.id) ?? [])];
    for (const child of queue) {
      if (seenChildren.has(child.id)) continue;
      seenChildren.add(child.id);
      subs.push(child);
      queue.push(...(children.get(child.id) ?? []));
    }
    const a = withKilnSubagents(agent, subs);
    const stand = standOf(a, seen[a.id]);
    const asking = stand === "needs" ? undefined : subs.find((s) => standOf(s, undefined) === "needs");
    const updated = updatedAt(a) > (seen[a.id] ?? 0);
    const requests = requestsFor([agent, ...subs]);
    return asking ? { a, stand: "needs", asking, updated, requests } : { a, stand, updated, requests };
  });
}

/** Plan 01a199e1b177752e84dad5dc990e6a15: status describes work; Updated describes attention. */
export function standOf(a: AgentInfo, _seen?: number): MissionStand {
  return chatStand(a);
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

/** Real return times, never a routine status/usage refresh. Subagents contribute to their row. */
export function updatedAt(a: AgentInfo): number {
  return Math.max(a.last_turn?.at ?? 0, ...(a.subagents ?? []).map((s) => s.ended ?? 0));
}

/** The row's time follows meaningful work and requests; reconnect timestamps never redate it. */
export function activityAt(a: AgentInfo): number {
  return Math.max(updatedAt(a), a.turn_started ?? 0, a.held?.since ?? 0, a.pause?.since ?? 0, a.exit?.at ?? 0, ...a.approvals.map((p) => p.at), ...(a.subagents ?? []).map((s) => s.started));
}
