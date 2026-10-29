/**
 * The copies runners keep of other runners' trees (docs/reach.md §5.1), in words and in a form's
 * terms: a scope's rules once the scopes above fill in what it leaves unset, schedules and hours
 * as people say them, and a form's draft to rules and back. Pure, so it's tested
 * (tests/keep-words.test.ts).
 */
import type { KeepRules, KeepTrigger } from "@/api/types";

/** `over`, with what it leaves unset taken from `under` (the scope above). */
export function merge(over: KeepRules, under: KeepRules): KeepRules {
  const out: KeepRules = { ...under };
  for (const [k, v] of Object.entries(over) as [keyof KeepRules, KeepRules[keyof KeepRules]][]) {
    if (v !== undefined && v !== null) (out as Record<string, unknown>)[k] = v;
  }
  return out;
}

/** Hours as people say them: "an hour", "36 hours", "a day", "7 days". */
export function hoursSaid(h: number): string {
  if (h === 1) return "an hour";
  if (h === 24) return "a day";
  if (h % 24 === 0) return `${h / 24} days`;
  return `${h} hours`;
}

/** Schedules offered by name. */
export const SCHEDULES: { cron: string; label: string }[] = [
  { cron: "0 * * * *", label: "Every hour" },
  { cron: "0 */6 * * *", label: "Every 6 hours" },
  { cron: "0 3 * * *", label: "Every day at 03:00" },
  { cron: "0 3 * * 0", label: "Every Sunday at 03:00" },
];

/** A schedule as people say it, its time zone after it. */
export function scheduleSaid(cron: string, timezone?: string): string {
  const named = SCHEDULES.find((s) => s.cron === cron.trim());
  const when = named ? named.label.charAt(0).toLowerCase() + named.label.slice(1) : `at \`${cron.trim()}\``;
  return `${when} (${timezone?.trim() || "UTC"})`;
}

/** Too little room, as rules set it: "10%", "50 GiB", "10% or 50 GiB"; null for none. */
export function roomSaid(r: KeepRules): string | null {
  const parts = [r.free_percent ? `${r.free_percent}%` : null, r.free_gib ? `${r.free_gib} GiB` : null].filter(Boolean);
  return parts.length ? parts.join(" or ") : null;
}

/** What rules (settled: every scope's filled in) say, in a sentence or two. */
export function rulesSaid(r: KeepRules): string {
  const n = r.newest ?? 0;
  const h = r.hours ?? 0;
  const keeps =
    n > 0 && h > 0
      ? r.both
        ? `The newest ${n} stay while each was used in the last ${hoursSaid(h)}`
        : `The newest ${n} stay, and any used in the last ${hoursSaid(h)}`
      : n > 0
        ? `The newest ${n} stay`
        : h > 0
          ? `Those used in the last ${hoursSaid(h)} stay`
          : "No rule keeps a copy";
  const room = roomSaid(r);
  const when = [r.schedule?.trim() ? scheduleSaid(r.schedule, r.timezone) : null, room ? `whenever free space falls under ${room}` : null].filter(Boolean);
  if (!when.length) return `${keeps}. Nothing goes on its own: no schedule or room limit is set.`;
  const rest = n > 0 || h > 0 ? "the rest go" : "every copy goes";
  const beyond = room && r.beyond ? " With too little room still, the least recently used of those kept go too, until there's room." : "";
  return `${keeps}; ${rest} ${when.join(", and ")}.${beyond} One in use, or used in the last ten minutes, always stays.`;
}

/** Why a copy went, in a word or two. */
export function triggerSaid(t: KeepTrigger): string {
  return t === "schedule" ? "on its schedule" : t === "room" ? "for room" : "asked to";
}

/* ---------------------------- the form ---------------------------- */

/** Each rule in a form: as the scope above says, none here, or set here. */
export type Mode = "inherit" | "none" | "set";

export interface Draft {
  newest: { mode: Mode; value: string };
  hours: { mode: Mode; value: string; unit: "hours" | "days" };
  both: "inherit" | "either" | "both";
  schedule: { mode: Mode; value: string };
  timezone: string;
  free_percent: { mode: Mode; value: string };
  free_gib: { mode: Mode; value: string };
  beyond: "inherit" | "no" | "yes";
}

/**
 * A form's draft of `r`. With nothing above (`top`: every runner's), what's unset is none.
 */
export function draftOf(r: KeepRules, top: boolean): Draft {
  const unset: Mode = top ? "none" : "inherit";
  const num = (v: number | undefined) => ({ mode: v === undefined ? unset : v === 0 ? ("none" as Mode) : ("set" as Mode), value: v ? String(v) : "" });
  const h = r.hours;
  const days = h !== undefined && h > 0 && h % 24 === 0;
  return {
    newest: num(r.newest),
    hours: { mode: h === undefined ? unset : h === 0 ? "none" : "set", value: h ? String(days ? h / 24 : h) : "", unit: days ? "days" : "hours" },
    both: r.both === undefined ? (top ? "either" : "inherit") : r.both ? "both" : "either",
    schedule: { mode: r.schedule === undefined ? unset : r.schedule.trim() ? "set" : "none", value: r.schedule?.trim() ?? "" },
    timezone: r.timezone ?? "",
    free_percent: num(r.free_percent),
    free_gib: num(r.free_gib),
    beyond: r.beyond === undefined ? (top ? "no" : "inherit") : r.beyond ? "yes" : "no",
  };
}

/** A whole number from 1 up to `max`, or null. */
function whole(v: string, max: number): number | null {
  const n = Number(v.trim());
  return Number.isInteger(n) && n >= 1 && n <= max ? n : null;
}

/**
 * The rules a draft says, or what's wrong with it. With nothing above (`top`), none is left unset:
 * it's what unset is there.
 */
export function rulesOf(d: Draft, top = false): { rules: KeepRules } | { error: string } {
  const r: KeepRules = {};
  const count = (f: { mode: Mode; value: string }, max: number, what: string): number | undefined | string => {
    if (f.mode === "inherit" || (top && f.mode === "none")) return undefined;
    if (f.mode === "none") return 0;
    return whole(f.value, max) ?? `${what}: a whole number from 1 to ${max}`;
  };
  const newest = count(d.newest, 100_000, "The newest");
  if (typeof newest === "string") return { error: newest };
  r.newest = newest;
  const hours = count(d.hours, d.hours.unit === "days" ? 3650 : 87_600, "Used within");
  if (typeof hours === "string") return { error: hours };
  r.hours = hours !== undefined && hours > 0 && d.hours.unit === "days" ? hours * 24 : hours;
  if (d.both !== "inherit" && !(top && d.both === "either")) r.both = d.both === "both";
  if (d.schedule.mode === "none" && !top) r.schedule = "";
  if (d.schedule.mode === "set") {
    const cron = d.schedule.value.trim();
    if (cron.split(/\s+/).length !== 5) return { error: "The schedule: five fields (minute hour day month weekday), as `0 3 * * *`" };
    r.schedule = cron;
  }
  if (d.timezone.trim()) r.timezone = d.timezone.trim();
  const percent = count(d.free_percent, 95, "Free space");
  if (typeof percent === "string") return { error: percent };
  r.free_percent = percent;
  const gib = count(d.free_gib, 1_048_576, "Free GiB");
  if (typeof gib === "string") return { error: gib };
  r.free_gib = gib;
  if (d.beyond !== "inherit" && !(top && d.beyond === "no")) r.beyond = d.beyond === "yes";
  for (const k of Object.keys(r) as (keyof KeepRules)[]) if (r[k] === undefined) delete r[k];
  return { rules: r };
}
