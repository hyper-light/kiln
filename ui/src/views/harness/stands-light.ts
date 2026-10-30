/**
 * The rule under Mission control's head (stands.tsx): a string of light with a stretch for each way
 * your agents stand, as long as its share of them, and a gap between each two.
 *
 * One string runs the length of the rule, held at its two ends. Its own motion is a few waves that
 * travel it at one speed, both ways, as a string carries every wave, each wave's strength and
 * period wandering slowly at random, so it never comes round the same; and the whole swells and
 * calms with its weather. Each state damps it as much as it's still: working hardly at all, offline
 * wholly, and the finer waves the more. Now and then a ripple comes in at one end and runs through
 * the string to the other, or a kick somewhere sends one each way. Each stretch is a medium of its
 * own: the livelier, the faster a ripple runs through it and the higher it rises there, so a
 * ripple slows and sinks through the calm stretches and quickens and rises through the lively
 * ones, one perturbation through the whole string. Most are soft and a few violent; however hard,
 * the string keeps to its box. Across each gap the damping and the medium pass smoothly from one
 * stretch to the next, so the string goes on unbroken.
 *
 * Each stretch is particles about the string, as a lens sees them, each unlike the next in size,
 * light, colour and liveliness: a fine core in focus on a narrow ribbon that turns about the
 * string, a soft layer about it, a few large out-of-focus discs, and dust furthest out, the
 * further from the string the later following it. Their colours are the ring the agent's pause
 * button wears in that state (console.css `--ag-rim`), running along the stretch while its glints
 * pass the other way. Paused gathers into beads, stopped into dashes, offline into still points.
 * At each gap both ends reach for each other, never touching.
 *
 * `frame` is pure, so tests/stands-light.test.ts checks it; `paint` draws one.
 */
import type { MissionStand } from "./mission-rows";

export type Shown = Exclude<MissionStand, "ended">;

/** Liveliest first. */
export const LIVELINESS: readonly Shown[] = ["working", "ready", "needs", "paused", "finished", "crashed", "offline"];

/** Each state's colour (its badge's), and working's ring (console.css, "active") for its badge's arc. */
export const LOOK: Record<Shown, { colour: string }> = {
  working: { colour: "#bab0e3" },
  ready: { colour: "#6f8f84" },
  needs: { colour: "#d8bb95" },
  paused: { colour: "#a6b2c6" },
  finished: { colour: "#a8c9bc" },
  crashed: { colour: "#d1aaad" },
  offline: { colour: "#7e818a" },
};
export const RING: readonly (readonly [colour: string, at: number])[] = [
  ["#b8d9d2", 0],
  ["#7194c9", 0.08],
  ["#c8e0ef", 0.13],
  ["#a6c4ed", 0.23],
  ["#8677ad", 0.32],
  ["#d7c1ee", 0.37],
  ["#b8a6d5", 0.46],
  ["#d6a2aa", 0.55],
  ["#eed6bc", 0.59],
  ["#a2bfae", 0.66],
  ["#d8cbb0", 0.77],
  ["#a6c4ed", 0.85],
  ["#d6a2aa", 0.91],
  ["#b8d9d2", 1],
];

/** The rule's box is HEIGHT tall, the string's rest at Y: room above and below for what floats about it and for its swings (stands.css gives back what it takes past the line's own room). */
export const HEIGHT = 44;
export const Y = 22;
/** Points between two stretches, and the least a stretch is. */
const GAP = 5;
const MIN = 12;
/** How long a change of counts takes to settle into the stretches' lengths (ms). */
export const GROW_MS = 600;

const TAU = 2 * Math.PI;
const smooth = (x: number) => (x <= 0 ? 0 : x >= 1 ? 1 : x * x * (3 - 2 * x));
const frac = (v: number) => v - Math.floor(v);
export const easeOut = (x: number) => 1 - (1 - Math.min(1, Math.max(0, x))) ** 3;

/** A stretch: where its particles are (`x0` to `x1`), and its region of the string (`r0` to `r1`: from the middle of the gap before it to the middle of the gap after, or the rule's end). */
export type Part = { stand: Shown; x0: number; x1: number; r0: number; r1: number };

/**
 * The stretches across `w`, in order. A count under 1 is a stretch coming or going: it grows from
 * nothing, and so does one gap beside it, so once it's gone its neighbours are one gap apart, as
 * they'll stay.
 */
export function stretches(stands: readonly Shown[], counts: readonly number[], w: number): Part[] {
  const present = stands.map((_, i) => Math.min(1, Math.max(0, counts[i] ?? 0)));
  // The gap before each stretch: as there as it is, and as the most there of those before it.
  let before = 0;
  const gaps = present.map((p, i) => {
    const g = i === 0 ? 0 : GAP * Math.min(p, before);
    before = Math.max(before, p);
    return g;
  });
  const fixed = present.reduce((s, p) => s + MIN * p, 0) + gaps.reduce((s, g) => s + g, 0);
  const total = stands.reduce((s, _, i) => s + Math.max(0, counts[i] ?? 0), 0) || 1;
  const free = Math.max(0, w - fixed);
  let x = 0;
  const parts = stands.map((stand, i): Part => {
    x += gaps[i] ?? 0;
    const len = MIN * (present[i] ?? 0) + (free * Math.max(0, counts[i] ?? 0)) / total;
    const p = { stand, x0: x, x1: x + len, r0: 0, r1: w };
    x += len;
    return p;
  });
  let prev: Part | undefined;
  for (const p of parts) {
    if (prev) {
      const mid = (prev.x1 + p.x0) / 2;
      prev.r1 = mid;
      p.r0 = mid;
    }
    prev = p;
  }
  return parts;
}

/**
 * A count's cells, two places at least, so 9 going to 10 moves nothing beside it: each place's
 * digit then and now (blank for none), and the way a changed one rolls: up as the count rises,
 * down as it falls. `start`: the digits begin the slot (a phone, its room trailing); `end`: they
 * end it, against the state's name (a desktop), each place under its own.
 */
export function cells(from: number, to: number, align: "start" | "end"): { was: string; now: string; dir: "up" | "down" }[] {
  const places = Math.max(2, String(from).length, String(to).length);
  const pad = (v: number) => (align === "end" ? String(v).padStart(places, " ") : String(v).padEnd(places, " "));
  const a = pad(from);
  const b = pad(to);
  const dir = to >= from ? "up" : "down";
  return [...b].map((now, i) => ({ was: (a[i] ?? " ").trim(), now: now.trim(), dir }));
}

const hash = (n: number) => {
  let x = Math.imul((n | 0) ^ 0x9e3779b9, 0x85ebca6b);
  x ^= x >>> 13;
  x = Math.imul(x, 0xc2b2ae35);
  x ^= x >>> 16;
  return (x >>> 0) / 4294967296;
};
/** Smooth noise in -1 to 1 along `u` (a new value at each whole `u`), and two octaves of it. */
const noise = (seed: number, u: number) => {
  const i = Math.floor(u);
  const g = u - i;
  const k = g * g * g * (g * (g * 6 - 15) + 10);
  return 2 * (hash(seed + i) * (1 - k) + hash(seed + i + 1) * k) - 1;
};
const wander = (seed: number, u: number) => 0.7 * noise(seed, u) + 0.3 * noise(seed + 7777, 2.7 * u + 0.5);

// ---- The string ---------------------------------------------------------------------------------

/** Points a second the string's own waves travel it. */
const SPEED = 32;
/** The string's own waves: the seconds one takes to pass, which way it goes, how big it is undamped, and how hard damping takes it (the finer, the harder). */
const WAVES: readonly (readonly [period: number, dir: number, amp: number, damp: number])[] = [
  [8 / 3, 1, 0.9, 1],
  [2, -1, 0.75, 1.3],
  [4 / 3, 1, 0.55, 1.8],
  [8 / 9, -1, 0.35, 2.4],
  [8 / 13, 1, 0.2, 3.2],
];
/** How far each wave's strength wanders (a share) and its phase (radians), over how many of its periods: each wave's period drifts about its own, so the string never comes round the same. */
const WANDER = { strength: 0.6, phase: 3.5, over: 3 } as const;
/** The string's weather: its motion swells and calms, from under half as much to over twice as much, the gusts running along it. */
const weather = (t: number) => Math.exp(1.1 * wander(9001, t / 6)) / 1.08;
/** How free each state leaves the string: 1 undamped, 0 still. */
export const LIVE: Record<Shown, number> = { working: 1, ready: 0.75, needs: 0.62, paused: 0.42, finished: 0.3, crashed: 0.15, offline: 0 };
/** Each state's medium: how fast a ripple runs through it (points a second), the livelier the faster. A ripple keeps its period as it goes, so in a slower stretch its crests come closer. */
export const MEDIUM: Record<Shown, number> = { working: 230, ready: 165, needs: 140, paused: 85, finished: 62, crashed: 46, offline: 36 };
/**
 * The ripples: a slot every so many seconds, and in each the chance of one coming in at an end and
 * of a kick (more in a storm); its period (seconds, spread evenly in their log), how hard it is
 * (most soft, a few violent: a power law, past `least`), and how far it carries (seconds of travel
 * to fade by e). Each is a train of its own one to four crests.
 */
const RIPPLE = { every: 0.5, ends: 0.14, kicks: 0.16, periods: [0.28, 2.6], least: 0.55, tail: 1.2, most: 8, fades: [5, 12] } as const;
/** However hard it's struck, the string keeps to its box: past the knee it gives less and less. */
const KNEE = 4.5;
const LIMIT = 9.5;
const limit = (y: number) => {
  const a = Math.abs(y);
  return a <= KNEE ? y : Math.sign(y) * (KNEE + (LIMIT - KNEE) * Math.tanh((a - KNEE) / (LIMIT - KNEE)));
};
/** Each line state's ribbon, its half width about the string. */
const HALF: Partial<Record<Shown, number>> = { working: 3, ready: 2.4, needs: 2.1, finished: 1.4 };
/** Half a point between the string's samples; the strings the far (and softer) particles follow are sampled every four. */
const STEP = 0.5;
const LATER_STEP = 4;
/** How far behind the string the far particles follow it (seconds): a strike leaves them trailing and swinging after it. */
const LATER = [0.05, 0.12, 0.22] as const;
/** Points between the samples of what changes slowly along the rule (each wave's wandering, the weather, the ribbon's turning). */
const SLOW = 4;

type Ripple = { t0: number; t0At: number; x0: number; kick: boolean; dir: number; period: number; crests: number; size: number; fade: number; damp: number };

/** The ripples on the string at `t`: `arrival(x)` the seconds one takes from the rule's start to x, through each medium, and `whole` across it. */
function ripplesAt(t: number, w: number, arrival: (x: number) => number, whole: number): Ripple[] {
  const out: Ripple[] = [];
  const [p0, p1] = RIPPLE.periods;
  for (let k = Math.floor((t - 26) / RIPPLE.every); k <= Math.floor(t / RIPPLE.every); k++) {
    const s0 = 9157 * k + 108;
    const t0 = (k + hash(s0)) * RIPPLE.every;
    if (t0 > t) continue;
    const wx = weather(t0);
    const ends = Math.min(0.6, RIPPLE.ends * wx);
    const kicks = Math.min(0.9 - ends, RIPPLE.kicks * wx);
    const roll = hash(s0 + 1);
    if (roll >= ends + kicks) continue;
    const kick = roll >= ends;
    const x0 = kick ? (0.08 + 0.84 * hash(s0 + 2)) * w : hash(s0 + 2) < 0.5 ? 0 : w;
    const period = p0 * (p1 / p0) ** hash(s0 + 3);
    const crests = 1 + 3 * hash(s0 + 4) ** 1.5;
    if (t - t0 > whole + 3 * crests * period + 1) continue;
    const size = Math.min(RIPPLE.most, RIPPLE.least * (1 - hash(s0 + 5)) ** (-1 / RIPPLE.tail)) * Math.sqrt(wx) * (hash(s0 + 6) < 0.5 ? -1 : 1);
    const fade = RIPPLE.fades[0] + (RIPPLE.fades[1] - RIPPLE.fades[0]) * hash(s0 + 7);
    // Finer ripples are taken harder by a calm medium, as the finer waves are.
    const damp = 0.6 + 1.2 * Math.min(1, Math.max(0, (2.2 - period) / 1.9));
    out.push({ t0, t0At: arrival(x0), x0, kick, dir: x0 === 0 ? 1 : -1, period, crests, size, fade, damp });
  }
  return out;
}

/** The first of `table`'s samples (rising) at or past `v`. */
function firstFrom(table: Float64Array, v: number) {
  let lo = 0;
  let hi = table.length;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if ((table[mid] ?? 0) < v) lo = mid + 1;
    else hi = mid;
  }
  return lo;
}
/** Arrays the string is worked out in, kept from frame to frame by a frame that asks for them (a test's string has arrays of its own). */
type Scratch = Float64Array[];
/** Slot `slot` of `pool` as `n` zeros, or a new array without a pool. */
function table(pool: Scratch | undefined, slot: number, n: number) {
  if (!pool) return new Float64Array(n);
  let a = pool[slot];
  if (!a || a.length < n) pool[slot] = a = new Float64Array(Math.ceil(n * 1.25) + 8);
  const v = a.subarray(0, n);
  v.fill(0);
  return v;
}
/** Reads a table of samples `step` apart at any x, between them. */
const reader = (samples: Float64Array, step: number) => (x: number) => {
  const n = samples.length;
  const f = Math.min(n - 1, Math.max(0, x / step));
  const k = Math.floor(f);
  return (samples[k] ?? 0) + ((samples[Math.min(n - 1, k + 1)] ?? 0) - (samples[k] ?? 0)) * (f - k);
};
/** What changes slowly along the rule, worked out every SLOW points from x 0 into `into` and read between. */
function slow(of: (x: number) => number, into: Float64Array) {
  for (let g = 0; g < into.length; g++) into[g] = of(g * SLOW);
  return reader(into, SLOW);
}
/** The string's own waves' wandering and its weather along the rule at `t`: slow enough that a moment's string and those a moment behind it share them. */
type Slow = { strength: ((x: number) => number)[]; phase: ((x: number) => number)[]; gusts: (x: number) => number };
function slowAt(w: number, t: number, pool: Scratch | undefined): Slow {
  const m = Math.ceil(w / SLOW) + 2;
  let slot = 40;
  const next = () => table(pool, slot++, m);
  return {
    strength: WAVES.map(([period, dir], j) => slow((x) => 1 + WANDER.strength * wander(1009 * j + 1, (t - (dir * x) / SPEED) / (WANDER.over * period)), next())),
    phase: WAVES.map(([period, dir], j) => slow((x) => period * 2.1 + WANDER.phase * wander(1009 * j + 503, (t - (dir * x) / SPEED) / (WANDER.over * period) + 0.37), next())),
    gusts: slow((x) => weather(t - x / (2 * SPEED)), next()),
  };
}
/** Each wave's damping where the string is as free as it was at the sample before (it changes only about the gaps). */
const DAMPED = new Float64Array(WAVES.length);

/** The string's heights across `w` at `t`, a sample every `step` points, its slow parts `sl`, in `pool`'s slots from `base`; and with `withHalf` the ribbon's half width at each. */
function sampled(parts: readonly Part[], w: number, t: number, step: number, sl: Slow, pool: Scratch | undefined, base: number, withHalf = false) {
  const n = Math.max(2, Math.ceil(w / step) + 2);
  const heights = table(pool, base, n);
  const free = table(pool, base + 1, n);
  const arrival = table(pool, base + 2, n);
  const half = withHalf ? table(pool, base + 3, n) : undefined;
  const reach = parts.slice(1).map((p, i) => {
    const a = parts[i];
    return a ? Math.max(0.5, Math.min(13, (a.r1 - a.r0) / 2.5, (p.r1 - p.r0) / 2.5)) : 0.5;
  });
  // What the stretches give at x, from the one whose region holds it, eased into its neighbour's
  // about each gap's middle. The samples go left to right, so the region does too.
  let i = 0;
  const across = (x: number, of: (p: Part) => number) => {
    while (i < parts.length - 1 && x >= (parts[i]?.r1 ?? w)) i++;
    const p = parts[i];
    if (!p) return 0;
    const before = parts[i - 1];
    const after = parts[i + 1];
    const hb = reach[i - 1] ?? 0;
    const ha = reach[i] ?? 0;
    if (before && x < p.r0 + hb) return of(before) + (of(p) - of(before)) * smooth((x - (p.r0 - hb)) / (2 * hb));
    if (after && x > p.r1 - ha) return of(p) + (of(after) - of(p)) * smooth((x - (p.r1 - ha)) / (2 * ha));
    return of(p);
  };
  for (let k = 0; k < n; k++) {
    const x = k * step;
    free[k] = across(x, (p) => LIVE[p.stand]);
    const speed = across(x, (p) => MEDIUM[p.stand]);
    if (half) half[k] = across(x, (p) => HALF[p.stand] ?? 0);
    if (k > 0) arrival[k] = (arrival[k - 1] ?? 0) + step / speed;
  }
  // The string's own waves, each wandering, in its weather.
  let was = -1;
  for (let k = 0; k < n; k++) {
    const x = k * step;
    const f = free[k] ?? 0;
    if (f <= 0) continue;
    if (f !== was) {
      WAVES.forEach(([, , , damp], j) => (DAMPED[j] = f ** damp));
      was = f;
    }
    let y = 0;
    WAVES.forEach(([period, dir, amp], j) => {
      y += amp * (sl.strength[j]?.(x) ?? 1) * (DAMPED[j] ?? 0) * Math.sin(TAU * (x / (SPEED * period) - (dir * t) / period) + (sl.phase[j]?.(x) ?? 0));
    });
    heights[k] = y * sl.gusts(x);
  }
  // The ripples, each only where it is: a crest reaches x once it has run there through the media
  // between, and it's gone once its train has passed.
  for (const r of ripplesAt(t, w, reader(arrival, step), arrival[n - 1] ?? 0)) {
    const since = t - r.t0;
    const train = 3 * r.crests * r.period;
    const add = (from: number, to: number) => {
      for (let k = Math.max(0, from); k < Math.min(n, to); k++) {
        const f = free[k] ?? 0;
        if (f <= 0) continue;
        const gone = Math.abs((arrival[k] ?? 0) - r.t0At);
        const s = since - gone;
        if (s <= 0 || s > train) continue;
        const e = r.damp * Math.log(f) - s / (0.6 * r.crests * r.period) - gone / r.fade;
        heights[k] = (heights[k] ?? 0) + r.size * smooth(s / (0.25 * r.period)) * Math.exp(e) * Math.sin((TAU * s) / r.period);
      }
    };
    // Where its train is, `since - train` to `since` seconds of travel from where it began: on
    // from there (a kick both ways, the point itself once).
    const near = Math.max(0, since - train);
    const k0 = firstFrom(arrival, r.t0At);
    if (r.kick || r.dir > 0) add(Math.max(k0, firstFrom(arrival, r.t0At + near) - 1), firstFrom(arrival, r.t0At + since) + 1);
    if (r.kick || r.dir < 0) add(firstFrom(arrival, r.t0At - since) - 1, Math.min(k0, firstFrom(arrival, r.t0At - near) + 1));
  }
  for (let k = 0; k < n; k++) heights[k] = smooth(Math.min(k * step, w - k * step) / 14) * limit(heights[k] ?? 0);
  return { heights, half, arrival };
}

/** The string at one moment: its height above its rest at x, the ribbon's half width there and its turning, the seconds a ripple takes to reach x from the rule's start through each medium, and the string as it was a moment ago (`later[i]`, LATER[i] seconds behind), which the far particles follow. */
export type Strung = { at: (x: number) => number; half: (x: number) => number; twist: (x: number) => number; arrival: (x: number) => number; later: readonly ((x: number) => number)[] };

/** The string across `w` at `t` (seconds), held at the rule's ends, each stretch damping it in its region and carrying ripples as its medium does, both passing smoothly across each gap's middle (up to 13 points either side). `pool`: arrays to work it out in, kept by a frame. */
export function stringOf(parts: readonly Part[], w: number, t: number, pool?: Scratch): Strung {
  const sl = slowAt(w, t, pool);
  const now = sampled(parts, w, t, STEP, sl, pool, 0, true);
  return {
    at: reader(now.heights, STEP),
    half: reader(now.half ?? new Float64Array(2), STEP),
    twist: slow((x) => 1.2 * wander(4441, (t - x / SPEED) / 3), table(pool, 60, Math.ceil(w / SLOW) + 2)),
    arrival: reader(now.arrival, STEP),
    later: LATER.map((lag, j) => reader(sampled(parts, w, t - lag, LATER_STEP, sl, pool, 4 * (j + 1)).heights, LATER_STEP)),
  };
}

// ---- Light --------------------------------------------------------------------------------------

/**
 * Each state's ring: the metal it runs dark and light round (console.css `--ag-rim`: working's
 * "active", stopped's, finished's, offline's "disconnected"; ready, needs you and paused have none
 * there, theirs made the same way from their colour), its glint's colour and strength, and the
 * seconds its colours and its glints take to go round.
 */
const RIM: Record<Shown, { stops: readonly (readonly [string, number])[]; glint: readonly [number, number, number, number]; current: number; spark: number }> = {
  working: { stops: RING, glint: [245, 234, 255, 0.9], current: 5.3, spark: 3.7 },
  ready: { stops: [["#5f7d73", 0], ["#86a79b", 0.22], ["#b3d0c5", 0.32], ["#6d8b80", 0.49], ["#93b2a6", 0.76], ["#5f7d73", 1]], glint: [220, 238, 231, 0.5], current: 9, spark: 7.2 },
  needs: { stops: [["#9e8664", 0], ["#d8bb95", 0.22], ["#efdcbd", 0.32], ["#ab926f", 0.49], ["#dcc5a1", 0.76], ["#9e8664", 1]], glint: [250, 236, 210, 0.45], current: 13, spark: 10 },
  paused: { stops: [["#717c90", 0], ["#a6b2c6", 0.22], ["#cdd6e5", 0.32], ["#808b9f", 0.49], ["#b2bdd0", 0.76], ["#717c90", 1]], glint: [222, 230, 242, 0.32], current: 20, spark: 16 },
  finished: { stops: [["#79978a", 0], ["#a8c9bc", 0.22], ["#d2e6d8", 0.32], ["#8aafa0", 0.49], ["#b4d0bd", 0.76], ["#79978a", 1]], glint: [225, 240, 229, 0.3], current: 32, spark: 27 },
  crashed: { stops: [["#996f77", 0], ["#d1aaad", 0.23], ["#e1bdc0", 0.3], ["#a17b83", 0.46], ["#c29ba1", 0.73], ["#996f77", 1]], glint: [233, 197, 200, 0.2], current: 52, spark: 43 },
  offline: { stops: [["#555962", 0], ["#797c84", 0.24], ["#898c94", 0.34], ["#656872", 0.53], ["#777a82", 0.8], ["#555962", 1]], glint: [163, 166, 173, 0.1], current: 100, spark: 83 },
};
const rgb = (h: string) => [1, 3, 5].map((i) => parseInt(h.slice(i, i + 2), 16));
const pack = (r: number, g: number, b: number) => (Math.round(r) << 16) | (Math.round(g) << 8) | Math.round(b);
/** Each ring's colours round it, 256 steps. */
const LUTS = Object.fromEntries(
  LIVELINESS.map((stand) => {
    const stops = RIM[stand].stops.map(([c, at]) => [rgb(c), at] as const);
    const lut = new Int32Array(256);
    for (let k = 0; k < 256; k++) {
      const u = k / 255;
      let j = 1;
      while (j < stops.length - 1 && u > (stops[j]?.[1] ?? 1)) j++;
      const [a, ua] = stops[j - 1] ?? stops[0]!;
      const [b, ub] = stops[j] ?? stops[0]!;
      const g = ub > ua ? Math.min(1, Math.max(0, (u - ua) / (ub - ua))) : 0;
      lut[k] = pack(...([0, 1, 2].map((n) => (a[n] ?? 0) + ((b[n] ?? 0) - (a[n] ?? 0)) * g) as [number, number, number]));
    }
    return [stand, lut];
  }),
) as Record<Shown, Int32Array>;
const mix = (c: number, r: number, g: number, b: number, k: number) => pack(((c >> 16) & 255) + (r - ((c >> 16) & 255)) * k, ((c >> 8) & 255) + (g - ((c >> 8) & 255)) * k, (c & 255) + (b - (c & 255)) * k);
const dim = (c: number, k: number) => pack(((c >> 16) & 255) * k, ((c >> 8) & 255) * k, (c & 255) * k);

/** The ring's shimmer (console.css `ag-rim-shimmer`, played in reverse): three glints, unevenly spaced, passing at changing speeds and strengths. */
const SHIMMER_O: readonly (readonly [number, number])[] = [[0, 0.45], [0.17, 1], [0.43, 0.3], [0.71, 0.8], [1, 0.45]];
const SHIMMER_R: readonly (readonly [number, number])[] = [[0, 0], [0.21, 41 / 360], [0.47, 192 / 360], [0.68, 243 / 360], [1, 1]];
const GLINTS = [0.07, 0.35, 0.73];
/** One turn of the ring laid along the line: this many points. */
const CYCLE = 110;
const keyed = (keys: readonly (readonly [number, number])[], p: number) => {
  for (let k = 1; k < keys.length; k++) {
    const [b, vb] = keys[k] ?? [1, 0];
    if (p <= b) {
      const [a, va] = keys[k - 1] ?? [0, 0];
      return va + ((vb - va) * (p - a)) / (b - a);
    }
  }
  return keys[keys.length - 1]?.[1] ?? 0;
};
const shimmer = (stand: Shown, t: number) => {
  const p = 1 - frac(t / RIM[stand].spark);
  return { o: keyed(SHIMMER_O, p) * RIM[stand].glint[3], rot: keyed(SHIMMER_R, p) };
};
/** A stretch's glints at a moment: how strong, and where round the ring each is. */
type Shine = { o: number; at: readonly number[] };
const shineOf = (stand: Shown, t: number): Shine => {
  const { o, rot } = shimmer(stand, t);
  return { o, at: GLINTS.map((g) => frac(g + rot)) };
};
function glintAt(sh: Shine, s: number) {
  const c = frac(s / CYCLE);
  let best = 0;
  for (const g of sh.at) {
    const d0 = Math.abs(c - g);
    best = Math.max(best, 1 - Math.min(d0, 1 - d0) / 0.03);
  }
  return smooth(best) * sh.o;
}
/** The ring's colour at `s` along a stretch at `t`, `grain` a particle's own lean round it, mixed toward the glint by `g`. */
function colourAt(stand: Shown, s: number, t: number, grain: number, g: number) {
  const c = LUTS[stand][Math.floor(frac(s / CYCLE - t / RIM[stand].current + grain) * 255)] ?? 0;
  const [r, gg, b] = RIM[stand].glint;
  return g > 0 ? mix(c, r, gg, b, Math.min(1, g)) : c;
}
const glintColour = (stand: Shown) => pack(RIM[stand].glint[0], RIM[stand].glint[1], RIM[stand].glint[2]);

// ---- Particles ----------------------------------------------------------------------------------

/**
 * How each state's particles move, liveliest first: how many a point in each layer (core, soft,
 * out of focus, dust); how far from the string the out-of-focus ones float; how far and how fast
 * each stirs on its own (two frequencies, a second); how fast they drift along (points a second,
 * the nearer the faster); the share of particles that now and then flash; and its light. Paused
 * gathers into beads (every so many points, so many particles each), stopped into dashes (on,
 * off), offline into still points.
 */
type Motion = {
  core: number;
  mid: number;
  bokeh: number;
  dust: number;
  spread: number;
  amp: number;
  freq: readonly [number, number];
  flow: number;
  sparkle: number;
  bright: number;
  pulse?: boolean;
  beads?: readonly [number, number];
  dashes?: readonly [number, number];
  every?: number;
};
const MOTION: Record<Shown, Motion> = {
  working: { core: 9, mid: 2.4, bokeh: 0.32, dust: 0.7, spread: 6.5, amp: 0.5, freq: [0.9, 1.6], flow: 9, sparkle: 0.08, bright: 1 },
  ready: { core: 8.2, mid: 1.9, bokeh: 0.24, dust: 0.54, spread: 5, amp: 0.4, freq: [0.6, 1.1], flow: 5, sparkle: 0.05, bright: 0.88 },
  needs: { core: 8.2, mid: 1.5, bokeh: 0.19, dust: 0.47, spread: 4.4, amp: 0.34, freq: [0.5, 0.9], flow: 3.2, sparkle: 0.04, bright: 0.94, pulse: true },
  paused: { core: 0, mid: 0, bokeh: 0, dust: 0, spread: 1.5, amp: 0.3, freq: [0.35, 0.6], flow: 0, sparkle: 0.024, bright: 0.85, beads: [6, 19] },
  finished: { core: 7.5, mid: 1, bokeh: 0.1, dust: 0.34, spread: 3.2, amp: 0.2, freq: [0.25, 0.45], flow: 1.2, sparkle: 0.02, bright: 0.8 },
  crashed: { core: 3.8, mid: 0, bokeh: 0, dust: 0, spread: 0.7, amp: 0.1, freq: [0.15, 0.3], flow: 0, sparkle: 0.006, bright: 0.7, dashes: [3.6, 2.6] },
  offline: { core: 0, mid: 0, bokeh: 0, dust: 0, spread: 0, amp: 0, freq: [0.1, 0.1], flow: 0, sparkle: 0, bright: 0.6, every: 6 },
};
const SEED: Record<Shown, number> = { working: 11, ready: 23, needs: 37, paused: 41, finished: 53, crashed: 67, offline: 71 };
const seedOf = (stand: Shown, k: number) => SEED[stand] * 7919 + k * 131;
/** Where along a stretch its ring starts, so each state's colours lie their own way. */
const offsetOf = (stand: Shown) => SEED[stand] * 37;
/** Needs you brightens and dims, so it's seen. */
export const calling = (t: number) => 0.8 + 0.2 * (0.5 - 0.5 * Math.cos((TAU * t) / 1.6));

/**
 * A frame: its crisp particles in focus (x, y, radius, opacity, colour; five numbers each) and its
 * soft light (x, y, diameter, opacity, colour, how much wider than tall; six each), added over
 * them as light adds, and the arrays its string is worked out in. Its arrays are kept from frame
 * to frame.
 */
export type Frame = { dots: number[]; soft: number[]; scratch: Float64Array[] };
export const newFrame = (): Frame => ({ dots: [], soft: [], scratch: [] });

/**
 * A particle's own lot, drawn once from its seed and kept: where along its stretch it starts, how
 * it drifts and stirs, how lively and how deep it is, and its light: its size (most fine, a few
 * large), how bright (dust to stars), how it twinkles, its lean round the ring and its tone (some
 * whiter, some deeper), and whether it now and then flashes.
 */
type Lot = {
  start: number;
  drift: number;
  live: number;
  f1: number;
  f2: number;
  p1: number;
  p2: number;
  blur: number;
  lane: number;
  spread: number;
  across: number;
  later: number;
  round: number;
  size: number;
  big: boolean;
  bright: number;
  twinkleF: number;
  twinkleP: number;
  twinkle: number;
  grain: number;
  tone: number;
  star: boolean;
  sparkle: number;
  flashF: number;
  flashP: number;
};
function lotOf(s0: number): Lot {
  return {
    start: hash(s0),
    drift: (0.6 + 0.8 * hash(s0 + 11)) * (0.4 + 1.2 * hash(s0 + 41)),
    live: 0.25 + 1.6 * hash(s0 + 40) ** 1.5,
    f1: 0.5 + 1.2 * hash(s0 + 2),
    f2: 0.5 + 1.2 * hash(s0 + 3),
    p1: hash(s0 + 4),
    p2: hash(s0 + 5),
    blur: hash(s0 + 20),
    lane: (hash(s0 + 21) + hash(s0 + 22) + hash(s0 + 23) - 1.5) / 1.5,
    spread: hash(s0 + 21) * 2 - 1,
    across: hash(s0 + 25) * 2 - 1,
    later: hash(s0 + 42),
    round: hash(s0 + 11),
    size: hash(s0 + 1),
    big: hash(s0 + 9) < 0.04,
    bright: hash(s0 + 10),
    twinkleF: 0.25 + 1.6 * hash(s0 + 6),
    twinkleP: hash(s0 + 7),
    twinkle: 0.1 + 0.5 * hash(s0 + 43),
    grain: (hash(s0 + 8) - 0.5) * 0.24,
    tone: hash(s0 + 44),
    star: hash(s0 + 45) < 0.025,
    sparkle: hash(s0 + 30),
    flashF: 0.14 + 0.3 * hash(s0 + 31),
    flashP: hash(s0 + 32),
  };
}
/** Each stand's particles' lots, layer by layer, made as many as are wanted and kept. */
const LOTS = new Map<number, Lot[]>();
function lots(stand: Shown, layer: number, n: number) {
  const key = seedOf(stand, layer * 4096);
  let got = LOTS.get(key);
  if (!got) LOTS.set(key, (got = []));
  for (let q = got.length; q < n; q++) got.push(lotOf(seedOf(stand, layer * 4096 + q)));
  return got;
}

/** A line particle's light: its colour from its ring where it is, leaning its own way and in its own tone; how bright (it twinkles as it does; a glint or a flash lifts it); how big. Dust (`layer` 3) is finest and faintest. */
const lit = { c: 0, r: 0, a: 0, flash: 0, star: false };
function light(p: Part, l: Lot, x: number, t: number, b: number, fade: number, pulse: number, layer: number, sh: Shine, shift = 0) {
  const m = MOTION[p.stand];
  const R = RIM[p.stand];
  const s = x - p.x0 + offsetOf(p.stand) + shift;
  const g = glintAt(sh, s);
  const flash = b === 0 && l.sparkle < m.sparkle ? Math.max(0, Math.sin(TAU * (t * l.flashF + l.flashP))) ** 28 : 0;
  const twinkle = 1 - l.twinkle * (0.5 + 0.5 * Math.sin(TAU * (t * l.twinkleF + l.twinkleP)));
  let c = colourAt(p.stand, s, t, l.grain, 0.6 * g);
  if (l.tone < 0.14) c = mix(c, R.glint[0], R.glint[1], R.glint[2], 0.35);
  else if (l.tone > 0.84) c = dim(c, 0.76);
  if (flash > 0) c = mix(c, R.glint[0], R.glint[1], R.glint[2], flash);
  const star = layer === 0 && l.star;
  lit.c = c;
  lit.r = layer === 3 ? 0.12 + 0.18 * l.size : (0.16 + 0.55 * l.size ** 2.2 + (l.big ? 0.45 : 0) + (star ? 0.25 : 0)) * (1 + 0.4 * g + 0.9 * flash);
  const base = layer === 3 ? 0.15 + 0.3 * l.bright : 0.32 + 0.68 * l.bright ** 0.8;
  lit.a = Math.min(1, m.bright * base * twinkle * (star ? 1.4 : 1) + 0.3 * g + 0.6 * flash) * fade * pulse;
  lit.flash = flash;
  lit.star = star;
  return lit;
}
/** The light of a stopped dash's or an offline point's particle, as it was: a steadier twinkle, its ring's own colour, now and then a flash. */
function steadyLight(p: Part, s0: number, x: number, t: number, sh: Shine) {
  const m = MOTION[p.stand];
  const R = RIM[p.stand];
  const s = x - p.x0 + offsetOf(p.stand);
  const g = glintAt(sh, s);
  const flash = hash(s0 + 30) < m.sparkle ? Math.max(0, Math.sin(TAU * (t * (0.14 + 0.3 * hash(s0 + 31)) + hash(s0 + 32)))) ** 28 : 0;
  const twinkle = 1 - 0.3 * (0.5 + 0.5 * Math.sin(TAU * (t * (0.35 + 1.1 * hash(s0 + 6)) + hash(s0 + 7))));
  const c = colourAt(p.stand, s, t, (hash(s0 + 8) - 0.5) * 0.08, 0.6 * g);
  lit.c = flash > 0 ? mix(c, R.glint[0], R.glint[1], R.glint[2], flash) : c;
  lit.r = (0.26 + 0.38 * hash(s0 + 1) ** 1.6 + (hash(s0 + 9) < 0.05 ? 0.3 : 0)) * (1 + 0.4 * g + 0.9 * flash);
  lit.a = Math.min(1, m.bright * (0.68 + 0.32 * hash(s0 + 10)) * twinkle + 0.3 * g + 0.6 * flash);
  lit.flash = flash;
  lit.star = false;
  return lit;
}
/** One particle: in focus (`b` 0) a crisp point; out of focus a soft disc, larger and fainter the further from focus, as a lens shows it. */
function particle(f: Frame, x: number, y: number, r: number, a: number, c: number, b: number) {
  if (a <= 0.01 || r <= 0) return;
  if (b === 0) f.dots.push(x, y, r, a, c);
  else f.soft.push(x, y, r * (3.5 + 12 * b), a * (0.85 - 0.35 * b), c, 1);
}
/** How far x is from its stretch's ends, eased: the ribbon and what floats about it narrow to the string at each end, so each stretch ends in a point, reaching. */
const taperOf = (p: Part, x: number, len: number) => smooth(Math.min(x - p.x0, p.x1 - x) / len);
const wrap = (p: Part, v: number) => {
  const L = p.x1 - p.x0;
  return p.x0 + ((((v - p.x0) % L) + L) % L);
};
/** The string a particle follows: the core most of it on the string itself, the soft layer and the out-of-focus discs a moment behind it, the dust the furthest behind. */
const followed = (S: Strung, layer: number, l: Lot) => (layer === 0 ? (l.later < 0.6 ? S.at : (S.later[0] ?? S.at)) : layer === 1 ? (S.later[1] ?? S.at) : (S.later[2] ?? S.at));

/** A line state's stretch: its core on the ribbon (the rule's, or `rb`), its soft, out-of-focus and dust layers floating about the string, and the sheen where each glint is. */
function line(f: Frame, p: Part, t: number, S: Strung, rb?: Ribbon) {
  const m = MOTION[p.stand];
  const pulse = m.pulse ? calling(t) : 1;
  const L = p.x1 - p.x0;
  const ends = Math.min(10, L / 2.4);
  const layers = [m.core, m.mid, m.bokeh, m.dust] as const;
  const sh = shineOf(p.stand, t);
  for (let layer = 0; layer < 4; layer++) {
    const n = Math.round(L * (layers[layer] ?? 0));
    const kept = lots(p.stand, layer, n);
    const drift = m.flow * [0.12, 0.25, 0.4, 0.18][layer]!;
    for (let q = 0; q < n; q++) {
      const l = kept[q]!;
      const f1 = m.freq[0] * l.f1;
      const f2 = m.freq[1] * l.f2;
      const x = wrap(p, p.x0 + l.start * L + drift * l.drift * t + (layer ? 0 : m.amp * 0.4 * l.live * Math.sin(TAU * (f2 * 0.5 * t + l.p2))));
      const tp = taperOf(p, x, ends);
      const fade = smooth(Math.min(x - p.x0, p.x1 - x) / (layer >= 2 ? 4 : 1.6));
      const stir = l.live * m.amp * (0.62 * Math.sin(TAU * (f1 * t + l.p1)) + 0.38 * Math.sin(TAU * (f2 * t + l.p2)));
      const sy = followed(S, layer, l)(x);
      let y: number;
      let size = 1;
      let glow = 1;
      let b = 0;
      let shift = 0;
      if (layer === 0) {
        // On the ribbon: across it, as it turns about the string (the rule's about a turn in 8 s,
        // unevenly, all along it; a compaction's turning on and on, each turn running along it),
        // its near side larger and brighter, and a compaction's face changing colour as it turns.
        const th = rb ? x * rb.along - (t * TAU) / rb.turn - S.twist(x) : x * 0.05 - (t * TAU) / 8 - S.twist(x);
        const depth = l.across * Math.sin(th);
        y = Y + sy + (l.across * (rb ? rb.half : S.half(x)) * Math.cos(th) + stir * 0.75) * tp;
        size = 1 + (rb ? 0.45 : 0.35) * Math.max(0, depth);
        glow = rb ? 0.66 + 0.34 * depth : 0.72 + 0.28 * depth;
        shift = rb ? rb.iris * depth : 0;
      } else {
        // Floating about the string, following it: most of the soft layer near it, the
        // out-of-focus discs anywhere within its spread, the dust further out.
        const off = (layer === 1 ? l.lane * m.spread * 0.5 : l.spread * m.spread * (layer === 2 ? 1 : 1.5)) * (rb ? rb.spread : 1);
        y = Y + sy * (layer === 3 ? 0.6 : 0.85) + (off + stir * (1 + Math.min(layer, 2))) * tp;
        b = layer === 1 ? 0.25 + 0.4 * l.blur : layer === 2 ? 0.65 + 0.35 * l.blur : 0;
      }
      const lt = light(p, l, x, t, b, fade, pulse, layer, sh, shift);
      particle(f, x, y, lt.r * size * (layer === 2 ? 1.4 : 1), lt.a * glow, lt.c, b);
      if (lt.flash > 0.05) f.soft.push(x, y, 7 * lt.r, 0.5 * lt.flash, lt.c, 1);
      if (lt.star) f.soft.push(x, y, 5 * lt.r, 0.35 * lt.a, lt.c, 1);
    }
  }
  // Each glint's sheen: a soft highlight along the string where it passes.
  const { o, rot } = shimmer(p.stand, t);
  const off = offsetOf(p.stand);
  for (const g of GLINTS) {
    const c = frac(g + rot);
    for (let k = Math.floor(off / CYCLE) - 1; ; k++) {
      const x = p.x0 + (k + c) * CYCLE - off;
      if (x > p.x1) break;
      if (x >= p.x0) f.soft.push(x, Y + S.at(x), 4.6, 0.55 * o * taperOf(p, x, ends), glintColour(p.stand), 2.8);
    }
  }
}
/** Paused: soft beads riding the string, each a small cloud of particles, unlike one another, turning about its middle, its outer ones following the string a moment behind. */
function beads(f: Frame, p: Part, t: number, S: Strung) {
  const m = MOTION[p.stand];
  const [every, per] = m.beads ?? [6, 19];
  const n = Math.max(1, Math.floor((p.x1 - p.x0 - 2) / every) + 1);
  const x0 = (p.x0 + p.x1 - (n - 1) * every) / 2;
  const kept = lots(p.stand, 8, n * 32);
  const sh = shineOf(p.stand, t);
  for (let bead = 0; bead < n; bead++) {
    const cx = x0 + bead * every;
    for (let q = 0; q < per; q++) {
      const l = kept[bead * 32 + q]!;
      const soft = q >= per - 2;
      const rad = q === 0 ? 0.1 : (0.25 + 0.95 * l.round ** 1.3) * (soft ? 1.4 : 1) * m.spread * 0.7;
      const f1 = m.freq[0] * (0.7 + 0.6 * (l.f1 - 0.5) / 1.2);
      const f2 = m.freq[1] * (0.7 + 0.6 * (l.f2 - 0.5) / 1.2);
      const cy = Y + (soft ? (S.later[2] ?? S.at) : q % 3 === 0 ? (S.later[0] ?? S.at) : S.at)(cx);
      const lt = light(p, l, cx, t, soft ? 0.5 : 0, 1, 1, soft ? 1 : 0, sh);
      particle(f, cx + rad * Math.cos(TAU * (f1 * t + l.p1)), cy + rad * Math.sin(TAU * (f2 * t + l.p2)), q === 0 ? 0.55 : lt.r, q === 0 ? Math.min(1, lt.a + 0.2) : lt.a, lt.c, soft ? 0.5 : 0);
    }
  }
}
/** Stopped: short dashes along the string, each packed with fine particles, barely stirring. */
function dashes(f: Frame, p: Part, t: number, S: Strung) {
  const m = MOTION[p.stand];
  const [on, off] = m.dashes ?? [3.6, 2.6];
  const sh = shineOf(p.stand, t);
  for (let d0 = p.x0 + 0.3, d = 0; d0 < p.x1 - 0.3; d0 += on + off, d++) {
    const len = Math.min(on, p.x1 - 0.3 - d0);
    const n = Math.max(2, Math.round(len * m.core));
    for (let q = 0; q < n; q++) {
      const s0 = seedOf(p.stand, d * 64 + q);
      const x = d0 + ((q + 0.5) * len) / n;
      const l = steadyLight(p, s0, x, t, sh);
      particle(f, x, Y + S.at(x) + (hash(s0 + 21) - 0.5) * m.spread + m.amp * Math.sin(TAU * (m.freq[0] * t + hash(s0 + 4))), l.r, l.a, l.c, 0);
    }
  }
}
/** Offline: single particles on the string, far apart, dim and still. */
function points(f: Frame, p: Part, t: number, S: Strung) {
  const every = MOTION[p.stand].every ?? 6;
  const n = Math.max(1, Math.floor((p.x1 - p.x0 - 1) / every) + 1);
  const x0 = (p.x0 + p.x1 - (n - 1) * every) / 2;
  const sh = shineOf(p.stand, t);
  for (let q = 0; q < n; q++) {
    const x = x0 + q * every;
    const s0 = seedOf(p.stand, q);
    particle(f, x, Y + S.at(x), 0.6 + 0.12 * hash(s0 + 1), 0.8, steadyLight(p, s0, x, t, sh).c, 0);
  }
}

/** How far each end reaches into gap `j` at `t`: out and back at random, each gap its own way, never past SHORT_OF_MIDDLE short of the gap's middle. */
export const reachOf = (j: number, t: number) => 0.35 + 1.25 * smooth(0.5 + 0.6 * wander(131 * j + 9, t / 1.3));
/** The least room left between a gap's middle and either end's reach. */
export const SHORT_OF_MIDDLE = 0.8;
/** At each gap both sides reach for each other along the string: from each end a few particles toward the gap's middle, the farthest the finest and faintest. */
function across(f: Frame, parts: readonly Part[], t: number, S: Strung) {
  parts.slice(1).forEach((b, j) => {
    const a = parts[j];
    if (!a) return;
    const mid = (a.x1 + b.x0) / 2;
    const e = reachOf(j, t);
    for (const [side, end, dir] of [
      [a, a.x1, 1],
      [b, b.x0, -1],
    ] as const) {
      if (side.x1 - side.x0 < 0.5) continue;
      const m = MOTION[side.stand];
      const reach = Math.max(0, Math.min(e, Math.abs(mid - end) - SHORT_OF_MIDDLE));
      const c = colourAt(side.stand, end - side.x0 + offsetOf(side.stand), t, 0, 0);
      for (let q = 1; q <= 4; q++) {
        const x = end + dir * reach * (q / 4);
        particle(f, x, Y + S.at(x) + m.amp * 0.45 * Math.sin(TAU * (m.freq[0] * t + q * 0.21 + j * 0.13)), 0.48 - q * 0.05, (0.95 - q * 0.16) * m.bright, c, 0);
      }
    }
  });
}

/**
 * A ribbon other than the rule's own, for a compaction (signals.tsx `Condense`): the rule's light on
 * a wider ribbon turning without end. Its half width (points), the seconds a turn takes and its
 * turning along each point (radians), how far its soft layers spread (a share of the state's own),
 * and how far round the ring a face's colour shifts as it turns to you (points: holographic). Every
 * layer keeps the rule's particles per point: the rule's light, at the rule's cost.
 */
export type Ribbon = { half: number; turn: number; along: number; spread: number; iris: number };
/** A compaction's: 9 points either side of the string, a turn every 2.8 s, about one every 140 points along. */
export const COMPACTING: Ribbon = { half: 9, turn: 2.8, along: 0.045, spread: 1.3, iris: 8 };

/** The rule across `w` at `t` (seconds) into `f`, as `paint` draws it; its line states on `ribbon` where one is given (a compaction's). */
export function frame(parts: readonly Part[], w: number, t: number, f: Frame = newFrame(), ribbon?: Ribbon): Frame {
  f.dots.length = 0;
  f.soft.length = 0;
  const S = stringOf(parts, w, t, f.scratch);
  for (const p of parts) {
    if (p.x1 - p.x0 <= 0.5) continue;
    const m = MOTION[p.stand];
    if (m.beads) beads(f, p, t, S);
    else if (m.dashes) dashes(f, p, t, S);
    else if (m.every) points(f, p, t, S);
    else line(f, p, t, S, ribbon);
  }
  across(f, parts, t, S);
  return f;
}

// ---- Drawing ------------------------------------------------------------------------------------

/** Colours kept to 5 bits a channel, opacities to twelfths: particles alike in both are drawn together, one fill each. */
const quantise = (c: number) => (((c >> 19) & 31) << 10) | (((c >> 11) & 31) << 5) | ((c >> 3) & 31);
const CSS = new Map<number, string>();
const cssOf = (q: number) => {
  let s = CSS.get(q);
  if (!s) CSS.set(q, (s = `rgb(${((q >> 10) & 31) * 8 + 4},${((q >> 5) & 31) * 8 + 4},${(q & 31) * 8 + 4})`));
  return s;
};
/** A soft disc for each colour (a gaussian falloff), made once and drawn at any size. */
const SPRITES = new Map<number, HTMLCanvasElement>();
function sprite(q: number) {
  let s = SPRITES.get(q);
  if (!s) {
    s = document.createElement("canvas");
    s.width = s.height = 32;
    const g = s.getContext("2d");
    if (g) {
      const fill = g.createRadialGradient(16, 16, 0, 16, 16, 16);
      const [r, gg, b] = [((q >> 10) & 31) * 8 + 4, ((q >> 5) & 31) * 8 + 4, (q & 31) * 8 + 4];
      for (const [at, a] of [
        [0, 1],
        [0.16, 0.8],
        [0.36, 0.4],
        [0.6, 0.12],
        [1, 0],
      ] as const)
        fill.addColorStop(at, `rgba(${r},${gg},${b},${a})`);
      g.fillStyle = fill;
      g.fillRect(0, 0, 32, 32);
    }
    SPRITES.set(q, s);
  }
  return s;
}
const LEVELS = 12;
/** Particles under this radius (points) are drawn as squares of their area: at a phone's or a desktop's scale, a pixel or two. */
const FINE = 0.42;
const groups = new Map<number, number[]>();
/** A frame into `ctx` (already scaled to points): its crisp particles in their own colours, then its soft light added over them. */
export function paint(ctx: CanvasRenderingContext2D, f: Frame) {
  for (const g of groups.values()) g.length = 0;
  const d = f.dots;
  for (let k = 0; k + 4 < d.length; k += 5) {
    const level = Math.round(Math.min(1, d[k + 3] ?? 0) * LEVELS);
    if (level === 0) continue;
    const key = quantise(d[k + 4] ?? 0) * (LEVELS + 1) + level;
    let g = groups.get(key);
    if (!g) groups.set(key, (g = []));
    g.push(d[k] ?? 0, d[k + 1] ?? 0, d[k + 2] ?? 0);
  }
  for (const [key, g] of groups) {
    if (g.length === 0) continue;
    ctx.globalAlpha = (key % (LEVELS + 1)) / LEVELS;
    ctx.fillStyle = cssOf(Math.floor(key / (LEVELS + 1)));
    ctx.beginPath();
    for (let k = 0; k + 2 < g.length; k += 3) {
      const x = g[k] ?? 0;
      const y = g[k + 1] ?? 0;
      const r = g[k + 2] ?? 0;
      // A particle about a pixel across is a pixel whatever its shape: a square of its area,
      // drawn for much less than a circle.
      if (r < FINE) {
        const h = r * 0.886;
        ctx.rect(x - h, y - h, 2 * h, 2 * h);
      } else {
        ctx.moveTo(x + r, y);
        ctx.arc(x, y, r, 0, TAU);
      }
    }
    ctx.fill();
  }
  ctx.globalCompositeOperation = "lighter";
  const s = f.soft;
  for (let k = 0; k + 5 < s.length; k += 6) {
    const a = s[k + 3] ?? 0;
    const size = s[k + 2] ?? 0;
    if (a <= 0.004 || size <= 0) continue;
    const wide = s[k + 5] ?? 1;
    ctx.globalAlpha = Math.min(1, a);
    ctx.drawImage(sprite(quantise(s[k + 4] ?? 0)), (s[k] ?? 0) - (size * wide) / 2, (s[k + 1] ?? 0) - size / 2, size * wide, size);
  }
  ctx.globalCompositeOperation = "source-over";
  ctx.globalAlpha = 1;
}
