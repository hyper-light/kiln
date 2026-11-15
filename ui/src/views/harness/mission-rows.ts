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

/** A row but for what your last look at it says (`updated`): its agent with its subagents, where it stands, who asks through it. */
type Family = Pick<MissionRow, "a" | "stand" | "asking" | "requests">;

const NO_AGENTS: readonly AgentInfo[] = [];
/** Each agent's row as last made, and the subagents it was made with. */
const made = new WeakMap<AgentInfo, { subs: readonly AgentInfo[]; family: Family }>();
/** Each list's rows as last made: Mission control's head and its list both ask, of the same list. */
const families = new WeakMap<readonly AgentInfo[], Family[]>();

/**
 * The rows of `list`, each agent with the subagents kiln started for it (and theirs). One pass over
 * the list, and an agent whose own word and whose subagents' haven't changed keeps the row it had
 * (the same objects), so a fleet's news costs the agents it's about, not all of them (W26-R15).
 */
function missionFamilies(list: readonly AgentInfo[]): Family[] {
  const known = families.get(list);
  if (known) return known;
  const ids = new Set<string>();
  for (const a of list) ids.add(a.id);
  const children = new Map<string, AgentInfo[]>();
  for (const a of list) {
    if (!a.subagent || !a.parent) continue;
    const of = children.get(a.parent);
    if (of) of.push(a);
    else children.set(a.parent, [a]);
  }
  const rows: Family[] = [];
  for (const agent of list) {
    if (agent.subagent && agent.parent && ids.has(agent.parent)) continue;
    let subs = NO_AGENTS;
    const own = children.get(agent.id);
    if (own) {
      const found: AgentInfo[] = [];
      const seenChildren = new Set([agent.id]);
      const queue = [...own];
      for (const child of queue) {
        if (seenChildren.has(child.id)) continue;
        seenChildren.add(child.id);
        found.push(child);
        const theirs = children.get(child.id);
        if (theirs) queue.push(...theirs);
      }
      subs = found;
    }
    const was = made.get(agent);
    if (was && was.subs.length === subs.length && was.subs.every((s, i) => s === subs[i])) {
      rows.push(was.family);
      continue;
    }
    const a = withKilnSubagents(agent, subs);
    const stand = standOf(a);
    const asking = stand === "needs" ? undefined : subs.find((s) => standOf(s) === "needs");
    const requests = requestsFor([agent, ...subs]);
    const family: Family = asking ? { a, stand: "needs", asking, requests } : { a, stand, requests };
    made.set(agent, { subs, family });
    rows.push(family);
  }
  families.set(list, rows);
  return rows;
}

/**
 * Each row as it stands (`seen`: when you last looked at each agent): its agent with the subagents
 * kiln started for it among its own, so it works through them as through its harness's own; and,
 * while it doesn't need you itself, the first of them that does, whose request is then its row's.
 */
export function missionRows(list: readonly AgentInfo[], seen: Readonly<Record<string, number>>): MissionRow[] {
  return missionFamilies(list).map((family) => ({ ...family, updated: updatedAt(family.a) > (seen[family.a.id] ?? 0) }));
}

/** Plan 01a199e1b177752e84dad5dc990e6a15: status describes work; Updated describes attention. */
export function standOf(a: AgentInfo, _seen?: number): MissionStand {
  return chatStand(a);
}

/** What an agent working through its subagents is doing: how many, and what each does. */
export const subagentsLine = (subs: readonly SubagentBrief[]): string => [subs.length === 1 ? "1 subagent" : `${subs.length} subagents`, ...subs.map((s) => s.description)].join(" · ");

const NO_SESSIONS: readonly string[] = [];
/** Which of the sessions asked about (`ids`) each agent is, or has as a subagent, as last told: an agent with news is a new object, and is asked again. */
let claims: { ids: string; of: WeakMap<AgentInfo, readonly string[]> } | undefined;

/** Sessions kiln runs itself, and their subagents, aren't listed again as sessions found on the machines. */
export function missionFoundSessions(found: AiSession[], managed: { a: AgentInfo }[]): AiSession[] {
  // None found that kiln doesn't say it runs: nothing to look for among every agent's subagents (W26-R15).
  const unclaimed = found.filter((session) => !session.managed_by);
  if (unclaimed.length === 0) return unclaimed;
  // Only those are looked for, and each agent is gone through once for them, not again at each word from another.
  const asked = new Set(unclaimed.map((session) => session.id));
  const ids = JSON.stringify([...asked].sort());
  if (claims?.ids !== ids) claims = { ids, of: new WeakMap() };
  const known = new Set<string>();
  for (const { a } of managed) {
    let mine = claims.of.get(a);
    if (!mine) {
      const own: string[] = [];
      if (a.harness_session && asked.has(a.harness_session)) own.push(a.harness_session);
      for (const task of a.subagents ?? []) if (task.id && asked.has(task.id)) own.push(task.id);
      mine = own.length > 0 ? own : NO_SESSIONS;
      claims.of.set(a, mine);
    }
    for (const id of mine) known.add(id);
  }
  return unclaimed.filter((session) => !known.has(session.id));
}

/** A subagent's events share its agent's conversation, but aren't Mission Control search results. */
export function missionSaid(hits: SaidHit[], agents: ReadonlySet<string>): SaidHit[] {
  return hits.filter((hit) => agents.has(hit.agent) && !hit.event.parent);
}

/** Each agent's times as last told: an agent with news is a new object, so one without has the times it had (W26-R15). */
const updatedAts = new WeakMap<AgentInfo, number>();
const activityAts = new WeakMap<AgentInfo, number>();

/** Real return times, never a routine status/usage refresh. Subagents contribute to their row. */
export function updatedAt(a: AgentInfo): number {
  let at = updatedAts.get(a);
  if (at === undefined) {
    at = a.last_turn?.at ?? 0;
    for (const s of a.subagents ?? []) at = Math.max(at, s.ended ?? 0);
    updatedAts.set(a, at);
  }
  return at;
}

/** The row's time follows meaningful work and requests; reconnect timestamps never redate it. */
export function activityAt(a: AgentInfo): number {
  let at = activityAts.get(a);
  if (at === undefined) {
    at = Math.max(updatedAt(a), a.turn_started ?? 0, a.held?.since ?? 0, a.pause?.since ?? 0, a.exit?.at ?? 0);
    for (const p of a.approvals) at = Math.max(at, p.at);
    for (const s of a.subagents ?? []) at = Math.max(at, s.started);
    activityAts.set(a, at);
  }
  return at;
}
