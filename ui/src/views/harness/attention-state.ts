/** Attention placement projection (plan 01a199e1b177752e84dad5dc990e6a15). No learned weights live in the UI. */
import type { AgentInfo } from "@/api/harness";
import type { AttentionCommand, AttentionMark, AttentionSettings, AttentionSnapshot } from "@/api/attention";
import type { MissionRow } from "./mission-rows";
import { requestsFor, requestsForFamily, requestsWakePark } from "@/lib/attention-requests";

export function attentionRows(rows: readonly MissionRow[], snapshot: AttentionSnapshot): MissionRow[] {
  const order = new Map(snapshot.order.map((id, i) => [id, i]));
  const requests = new Map(snapshot.request_order.map((id, i) => [id, i]));
  return rows.map((row) => {
    const mark = snapshot.agents[row.a.id];
    return {
      ...row,
      rank: order.get(row.a.id),
      pinned: mark?.pinned,
      reason: mark?.reason ?? mark?.rank_reason,
      correction: mark?.correction === true,
      requestRank: requests.get(row.a.id),
      stale: mark?.urgency?.stale === true,
      urgency: mark?.urgency,
    };
  });
}

/** A new request/error wakes a park; choosing Park can defer an already known request. */
export function isParked(row: MissionRow, snapshot: AttentionSnapshot, now: number): boolean {
  const mark = snapshot.agents[row.a.id];
  if (mark?.parked_until === undefined || mark.parked_until <= now) return false;
  const current = row.requests ?? requestsFor(row.asking ? [row.a, row.asking] : [row.a]);
  const unknown = current.unknown || ((row.stand === "needs" || row.stand === "crashed") && current.requests.length === 0);
  return !requestsWakePark({ ...current, unknown }, mark.deferred_requests);
}

export type ParkChoice = { value: "later" | "evening" | "tomorrow"; label: string; at: number };

/** The park menu's times, from your settings: local-day choices keep tomorrow morning across DST. */
export function parkChoices(now: number, settings: Pick<AttentionSettings, "park_later_minutes" | "park_evening_hour" | "park_morning_hour">): ParkChoice[] {
  const start = new Date(now);
  const evening = new Date(start);
  evening.setHours(settings.park_evening_hour, 0, 0, 0);
  const tomorrow = new Date(start);
  tomorrow.setDate(tomorrow.getDate() + 1);
  tomorrow.setHours(settings.park_morning_hour, 0, 0, 0);
  const later = now + settings.park_later_minutes * 60_000;
  const sameDay = new Date(later).toDateString() === start.toDateString();
  return [
    { value: "later", label: sameDay ? "Later today" : `In ${settings.park_later_minutes} minutes`, at: later },
    ...(evening.getTime() > now ? [{ value: "evening" as const, label: "This evening", at: evening.getTime() }] : []),
    { value: "tomorrow", label: "Tomorrow morning", at: tomorrow.getTime() },
  ];
}

const clock = (at: Date) => at.toLocaleTimeString(undefined, { hour: "numeric", minute: "2-digit" });
/** When a park ends, in words: "today at 3:20 PM", "tomorrow at 9:00 AM", "Fri 14 Nov at 9:00 AM". */
export function untilWords(at: number, now: number): string {
  const end = new Date(at);
  const today = new Date(now);
  const tomorrow = new Date(now);
  tomorrow.setDate(tomorrow.getDate() + 1);
  if (end.toDateString() === today.toDateString()) return `today at ${clock(end)}`;
  if (end.toDateString() === tomorrow.toDateString()) return `tomorrow at ${clock(end)}`;
  return `${end.toLocaleDateString(undefined, { weekday: "short", day: "numeric", month: "short" })} at ${clock(end)}`;
}

/** An hour of the day as this device says its times (the park menu's evening and morning). */
export const hourWords = (hour: number) => clock(new Date(2000, 0, 1, hour, 0, 0, 0));

/** What a row's request has waited, against how long you take to answer that kind (its urgency), in words. */
export function urgencyWords(urgency: NonNullable<AttentionMark["urgency"]>): string {
  if (urgency.stale) return "Waiting longer than any request you've answered";
  return urgency.reason;
}

/** What it shows at once while its request is on its way (W21-R15): the server's own answer replaces it. */
export function organizedLocally(snapshot: AttentionSnapshot, commands: readonly AttentionCommand[], now: number, agents: readonly AgentInfo[]): AttentionSnapshot {
  const marks = { ...snapshot.agents };
  let archived = snapshot.archived;
  let settings = snapshot.settings;
  const mark = (id: string): AttentionMark => marks[id] ?? { revision: 0 };
  for (const c of commands) {
    switch (c.action) {
      case "pin":
        marks[c.agent] = { ...withoutPark(mark(c.agent)), pinned: now, reason: undefined, correction: undefined };
        break;
      case "unpin":
        marks[c.agent] = { ...mark(c.agent), pinned: undefined };
        break;
      case "park": {
        const requests = requestsForFamily(agents, c.agent).requests;
        marks[c.agent] = {
          ...mark(c.agent),
          pinned: undefined,
          parked_at: now,
          parked_until: c.until,
          deferred_requests: requests,
          reason: "You parked it",
          automatic: undefined,
          correction: undefined,
        };
        break;
      }
      case "unpark": {
        // Bringing back a park kiln made is a correction (the server's words, harness/attention/mod.rs).
        const automatic = mark(c.agent).automatic === true;
        marks[c.agent] = {
          ...withoutPark(mark(c.agent)),
          reason: automatic ? "You brought it back: kiln had parked it automatically" : "You brought it back",
          correction: automatic || undefined,
          woke_at: now,
        };
        break;
      }
      case "seen":
        marks[c.agent] = { ...mark(c.agent), updated_seen: now };
        break;
      case "archive": {
        const info = agents.find((a) => a.id === c.agent);
        marks[c.agent] = { ...mark(c.agent), archived: true, archive_status: "archiving" };
        if (info && !archived.some((a) => a.id === c.agent)) archived = [info, ...archived];
        break;
      }
      case "restore":
      case "restore_copy":
        marks[c.agent] = { ...mark(c.agent), archived: undefined, archive_status: undefined };
        archived = archived.filter((a) => a.id !== c.agent);
        break;
      case "reorder":
        c.order.forEach((id, i) => {
          marks[id] = { ...mark(id), pinned: i + 1 };
        });
        break;
      case "settings":
        settings = { ...settings, ...c.settings };
        break;
      case "undo":
        break;
    }
  }
  return { ...snapshot, agents: marks, archived, settings };
}
const withoutPark = (m: AttentionMark): AttentionMark => ({ ...m, parked_at: undefined, parked_until: undefined, deferred_requests: undefined, automatic: undefined });

/* ---------------------------------------------------------------- the learned order, in words (Settings) */

const KINDS: Record<string, string> = {
  message: "your messages",
  answer: "your answers",
  command: "your commands",
  open: "opening it",
  pin: "pinning it",
  park: "parking it",
  unpark: "bringing it back",
  archive: "archiving it",
  restore: "restoring it",
  turn: "its turns",
  subagent_turn: "its subagents' turns",
};
/** A name the server gives, said as words when there's none of ours for it ("human_recency" → "Human recency"). */
const plainName = (name: string) => {
  const words = name.replace(/[._]+/g, " ").trim();
  return words.charAt(0).toUpperCase() + words.slice(1);
};
const kindWords = (kind: string) => KINDS[kind] ?? plainName(kind).toLowerCase();

const COMPARISONS: Record<string, string> = {
  act_r: "Learned order",
  act_r_with_range: "Learned order with time span",
  your_recency: "Your most recent use",
  without_agent_turns: "Learned from your uses only",
  latest_activity: "Latest activity (today's order)",
};
/** An order the learned one is compared with. */
export const comparisonLabel = (name: string) => COMPARISONS[name] ?? plainName(name);

/** One of the learned order's parameters. */
export function parameterLabel(name: string): string {
  if (name === "decay") return "Decay";
  if (name === "range_weight") return "Weight of time span";
  if (name.startsWith("weight.")) return `Weight of ${kindWords(name.slice("weight.".length))}`;
  if (name.startsWith("scale.")) return `Strength of ${kindWords(name.slice("scale.".length))}, relative to a pick`;
  return plainName(name);
}
