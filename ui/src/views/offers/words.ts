/**
 * Runners → Offers in words (docs/reach.md §2.1): amounts as people say them, a runner's answer
 * for an ask size, when it would start (in the unit the server shows it in, counted from when the
 * runner said it, so the page needn't change as time passes), and what waits. Pure, so it's
 * tested (tests/offers-words.test.ts).
 */
import type { OfferAmounts, OfferAnswer, OfferCount, OfferGroup, OfferRunner, OffersShown } from "@/api/wire";

const GiB = 1024 ** 3;
const MiB = 1024 ** 2;

/** Bytes as a whole or one-decimal number of GiB, or MiB under one. */
function bytesSaid(b: number): string {
  if (b >= GiB) {
    const g = b / GiB;
    return `${Number.isInteger(g) ? g : g.toFixed(1)} GiB`;
  }
  return `${Math.round(b / MiB)} MiB`;
}

/** CPUs from thousandths: "4 CPUs", "1 CPU", "0.5 CPU". */
export function cpusSaid(milli: number): string {
  const c = milli / 1000;
  const n = Number.isInteger(c) ? String(c) : String(Number(c.toFixed(3)));
  return `${n} ${c === 0 || c > 1 ? "CPUs" : "CPU"}`;
}

/** An amount in each dimension, the ones asked: "4 CPUs · 8 GiB", "1 GPU"; nothing at all: "any size". */
export function amountsSaid(a: OfferAmounts): string {
  const parts: string[] = [];
  if (a.cpu_milli > 0) parts.push(cpusSaid(a.cpu_milli));
  if (a.mem > 0) parts.push(bytesSaid(a.mem));
  if (a.disk > 0) parts.push(`${bytesSaid(a.disk)} disk`);
  if (a.gpu > 0) parts.push(`${a.gpu} ${a.gpu === 1 ? "GPU" : "GPUs"}`);
  return parts.length ? parts.join(" · ") : "any size";
}

const UNIT_WORDS: Record<OffersShown["unit"], [string, string]> = { s: ["second", "seconds"], min: ["minute", "minutes"], h: ["hour", "hours"], d: ["day", "days"] };

/** A start as the server shows it: "within a minute", "in about 3 minutes", "in about 2 days". */
export function startSaid(s: OffersShown): string {
  if (s.unit === "s") return "within a minute";
  const [one, many] = UNIT_WORDS[s.unit];
  return `in about ${s.count} ${s.count === 1 ? one : many}`;
}

/** Why a run waits, as the runner's admission or the CI engine names it, in words. */
const REASONS: Record<string, string> = {
  room: "not enough room free",
  kept: "room kept for the first in line",
  line: "runs ahead of it go first",
  whole: "it takes the runner whole",
  draining: "the runner is draining",
  away: "its owner is away",
  strained: "the runner is under strain",
  retry: "waiting to try again",
  capacity: "every runner that could is busy",
  "no-runner": "no runner that could is connected",
  capability: "no runner has what it needs",
  concurrency: "its concurrency group is full",
  quota: "its quota is used",
  exclusive: "it needs a runner to itself",
  paused: "paused",
  scaling: "its pool is starting executors",
  reservation: "room is kept for other work",
};

export function reasonSaid(r: string): string {
  return REASONS[r] ?? r.replace(/-/g, " ");
}

/** A runner's answer for one size: "starts now", "waits: runs ahead of it go first, starts in about 3 minutes", "never: past its budget of GPUs". */
export function answerSaid(a: OfferAnswer): string {
  if (a.answer === "now") return "starts now";
  if (a.answer === "never") return a.reason ? `never: ${a.reason}` : "never";
  const why = a.reason ? reasonSaid(a.reason) : "waits";
  const when = a.soonest ? `, starts ${startSaid(a.soonest)}` : "";
  const latest = a.latest && a.soonest && (a.latest.count !== a.soonest.count || a.latest.unit !== a.soonest.unit) ? ` (at the latest ${startSaid(a.latest)})` : "";
  return `waits: ${why}${when}${latest}`;
}

/** Counts by name: "2 interactive, 1 batch"; none: the empty string. */
export function countsSaid(c: OfferCount[], words: (k: string) => string = (k) => k): string {
  return c.map((x) => `${x.count} ${words(x.key)}`).join(", ");
}

/** A runner's line: "3 wait: 2 interactive, 1 batch", or "nothing waits". */
export function lineSaid(r: OfferRunner): string {
  const n = r.queued.reduce((a, c) => a + c.count, 0);
  if (n === 0) return "nothing waits";
  return `${n} ${n === 1 ? "waits" : "wait"}: ${countsSaid(r.queued)}`;
}

/** What a group needs: "linux/amd64 +gpu", or "any runner". */
export function needSaid(g: Pick<OfferGroup, "need">): string {
  return g.need.trim() || "any runner";
}

/** How many of a group wait, and why. */
export function groupSaid(g: OfferGroup): string {
  const runs = `${g.runs} ${g.runs === 1 ? "waits" : "wait"}`;
  // One reason for them all needs no count of its own.
  const only = g.reasons.length === 1 ? g.reasons[0] : undefined;
  const why = only ? reasonSaid(only.key) : countsSaid(g.reasons, reasonSaid);
  return why ? `${runs}: ${why}` : runs;
}

/** The status dot a runner's health shows. */
export function healthStatus(r: OfferRunner): string {
  if (!r.connected) return "cancelled";
  if (!r.health.taking) return "timed_out";
  return r.said ? "succeeded" : "queued";
}

/** A runner's health in words: "takes work", or why not. */
export function healthSaid(r: OfferRunner): string {
  if (r.health.taking && !r.health.why) return "takes work";
  return r.health.why ?? (r.health.taking ? "takes work" : "takes no work");
}

/** A runner's answers that start now, of all it gives. */
export function nowCount(r: OfferRunner): number {
  return r.answers.filter((a) => a.answer === "now").length;
}
