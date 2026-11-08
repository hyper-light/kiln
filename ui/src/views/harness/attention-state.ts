/** Attention placement projection (plan 01a199e1b177752e84dad5dc990e6a15). No learned weights live in the UI. */
import type { AttentionSettings, AttentionSnapshot } from "@/api/attention";
import type { MissionRow } from "./mission-rows";
import { requestsFor, requestsWakePark } from "@/lib/attention-requests";

export function attentionRows(rows: readonly MissionRow[], snapshot: AttentionSnapshot): MissionRow[] {
  const order = new Map(snapshot.order.map((id, i) => [id, i]));
  const requests = new Map((snapshot.request_order ?? []).map((id, i) => [id, i]));
  return rows.map((row) => {
    const mark = snapshot.agents[row.a.id];
    return {
      ...row,
      rank: order.get(row.a.id),
      pinned: mark?.pinned,
      reason: mark?.reason ?? mark?.rank_reason ?? mark?.wake_reason,
      requestRank: requests.get(row.a.id),
      stale: mark?.urgency?.stale === true,
      urgencyReason: mark?.urgency?.reason,
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

/** Settings, not model constants: local-day choices preserve tomorrow morning across DST. */
export function parkChoices(now: number, settings?: Pick<AttentionSettings, "park_later_minutes" | "park_evening_hour" | "park_morning_hour">): { value: string; label: string; at: number }[] {
  const start = new Date(now);
  const evening = new Date(start);
  evening.setHours(settings?.park_evening_hour ?? 18, 0, 0, 0);
  const tomorrow = new Date(start);
  tomorrow.setDate(tomorrow.getDate() + 1);
  tomorrow.setHours(settings?.park_morning_hour ?? 9, 0, 0, 0);
  const minutes = settings?.park_later_minutes ?? 60;
  const later = now + minutes * 60_000;
  const sameDay = new Date(later).toDateString() === start.toDateString();
  return [
    { value: "later", label: sameDay ? "Later today" : `In ${minutes} minutes`, at: later },
    ...(evening.getTime() > now ? [{ value: "evening", label: "This evening", at: evening.getTime() }] : []),
    { value: "tomorrow", label: "Tomorrow morning", at: tomorrow.getTime() },
  ];
}
