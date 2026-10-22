/** How the Programs view says things (docs/programs.md): pure, so tests read it without a page. */
import type { Program, ProgramAgent, ProgramChange, ProgramChangeSettings, ProgramGoal, ProgramSettings, ProgramTrack, TrackSlot } from "@/api/programs";
import { relativeTime } from "@/lib/format";

export type Tone = "ok" | "warn" | "fail" | "dim";

/** A track's slot in words, and its tone. */
export function slotWords(slot: TrackSlot): { text: string; tone: Tone } {
  switch (slot) {
    case "running":
      return { text: "at work", tone: "ok" };
    case "next":
      return { text: "next", tone: "ok" };
    case "waiting":
      return { text: "waiting", tone: "dim" };
    case "held":
      return { text: "held for usage", tone: "warn" };
    case "paused":
      return { text: "paused for usage", tone: "fail" };
    case "done":
      return { text: "done", tone: "dim" };
  }
}

/** A program's usage, and the rule it brings, in words (none: nothing to say). */
export function usageWords(p: Pick<Program, "usage" | "rule">): { text: string; tone: Tone } | null {
  if (!p.usage) return null;
  const pct = `${Math.round(p.usage.pct)}% of a weekly window`;
  switch (p.rule.rule) {
    case "pause":
      return { text: `${pct}: above 95%, the program's goal stacks pause and no new goal starts`, tone: "fail" };
    case "hold":
      return { text: `${pct}: above 80%, no new goal starts (the goals under way go on)`, tone: "warn" };
    default:
      return { text: `${pct}: under 80%, goals start as their turns come`, tone: "ok" };
  }
}

/** Why an agent's session is paused, and when it goes on: "paused at its session limit, starts over in 2h". */
export function pauseWords(a: Pick<ProgramAgent, "pause" | "pause_at">, now: number): string | null {
  const p = a.pause;
  if (!p) return null;
  const window = (p.label ?? p.window ?? "").toLowerCase();
  const at = p.reason === "threshold" ? `paused at its threshold${a.pause_at ? ` (${a.pause_at}%)` : ""}${window ? `, its ${window} window` : ""}` : `paused at its ${window ? `${window} ` : "usage "}limit`;
  const until = p.until ? `, starts over ${relativeTime(p.until / 1000, now)}` : "";
  return `${at}${until}`;
}

/** The threshold a track's worker pauses at, and kiln's rule for it, in words (none: no worker). */
export function thresholdWords(t: Pick<ProgramTrack, "worker" | "wants">): string | null {
  if (!t.worker) return null;
  const at = t.worker.pause_at;
  const rule = t.wants === 80 ? "80%: the lowest-weight track at work" : t.wants ? `${t.wants}%` : null;
  if (at === undefined) return rule ? `kiln sets it to pause at ${rule}` : "no threshold";
  if (t.wants === undefined || at === t.wants) return `pauses at ${rule ?? `${at}%`}`;
  return `pauses at ${at}% (kiln's rule: ${rule})`;
}

/** Where a track's goal stands: under way (on which agent, when it's a goal handed to it), or next. */
export function goalWords(g: Pick<ProgramGoal, "status" | "on" | "on_name">, slot: TrackSlot): string {
  if (g.on) return `Under way on ${g.on_name ?? g.on.slice(0, 8)}`;
  if (g.status === "active") return "Under way";
  return slot === "held" ? "Next, held" : "Next";
}

/** Whether the coordinator was told of the settings: kiln queued the message for its session, or why not yet. */
export function noticeWords(s: Pick<ProgramSettings, "notice">, now: number): { text: string; tone: Tone } | null {
  const n = s.notice;
  if (!n) return null;
  // A clock read before the notice went says it went now, not in a while.
  if (n.delivered) return { text: `The coordinator was told ${relativeTime(Math.min(n.at, now) / 1000, now)}: kiln queued the message for its session, and it follows them from its next goal boundary.`, tone: "ok" };
  return { text: `Not yet delivered to the coordinator${n.why ? `: ${n.why}` : ""}. kiln tries again every 15 s.`, tone: "warn" };
}

/** A change request in words: which, how far it went (built, merged or closed), and its checks. */
export function changeWords(c: ProgramChange): string {
  return [c.change ?? c.branch, c.state, c.checks ? `checks ${c.checks}` : null].filter(Boolean).join(" · ");
}

/** A pass, short. */
export function passWords(p: number): string {
  return Number.isInteger(p) ? String(p) : p.toFixed(2);
}

/** A weight or slot field's text as a whole number in its bounds, or why not. */
function whole(text: string, max: number, what: string): number | string {
  const t = text.trim();
  if (!/^\d+$/.test(t)) return `${what} is a whole number from 1 to ${max}`;
  const n = Number(t);
  return n >= 1 && n <= max ? n : `${what} is a whole number from 1 to ${max}`;
}

/** What the edited form changes (only what differs from the program now): the settings, and each
 * track whose worker changed (an agent's id, or null: none), or why it can't be kept. */
export function edited(
  p: Pick<Program, "slots" | "tracks">,
  slots: string,
  weights: Record<string, string>,
  workers: Record<string, string> = {},
): { body: ProgramChangeSettings; assign: Record<string, string | null>; error: string | null; changed: boolean } {
  const body: ProgramChangeSettings = {};
  const assign: Record<string, string | null> = {};
  const s = whole(slots, 32, "Slots");
  if (typeof s === "string") return { body, assign, error: s, changed: false };
  if (s !== p.slots) body.slots = s;
  for (const t of p.tracks) {
    const text = weights[t.id];
    if (text !== undefined) {
      const w = whole(text, 100, `${t.id}'s weight`);
      if (typeof w === "string") return { body, assign, error: w, changed: false };
      if (w !== t.weight) (body.weights ??= {})[t.id] = w;
    }
    const worker = workers[t.id];
    if (worker !== undefined && worker !== (t.assigned ?? "")) assign[t.id] = worker || null;
  }
  return { body, assign, error: null, changed: body.slots !== undefined || body.weights !== undefined || Object.keys(assign).length > 0 };
}
