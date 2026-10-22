/**
 * Mission Control lists every agent kiln runs, one row each: an agent started through kiln's API is
 * a top-level agent, whoever started it. A harness's own subagents stay in their agent's chat.
 */
import type { AgentInfo, SaidHit, SubagentBrief } from "@/api/harness";
import type { AiSession } from "@/api/types";
import { throughSubagents } from "./tasks";

export type MissionStand = "needs" | "crashed" | "paused" | "working" | "finished" | "ready" | "offline" | "ended";
export type MissionRow = { a: AgentInfo; stand: MissionStand };

/** Where an agent stands (`seen`: when you last looked at it). Its turn over, a subagent it sent off still at work keeps it working (tasks.ts). */
export function standOf(a: AgentInfo, seen: number | undefined): MissionStand {
  if (a.approvals.length > 0 || a.status === "waiting") return "needs";
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
