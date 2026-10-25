/**
 * The rule under Mission control's head (stands.tsx): a string of light with a stretch for each way
 * your agents stand, as long as its share of them, and a gap between each two.
 *
 * One string runs the length of the rule, held at its two ends. Its motion is a few waves that
 * travel it at one speed, both ways, as a string carries every wave. Each state damps it as much
 * as it's still: working hardly at all, offline wholly, and the finer waves the more, so the
 * calmer a stretch the smaller and slower its motion. Across each gap the damping passes smoothly
 * from one stretch to the next, so the string goes on unbroken, one motion all along it.
 *
 * Each stretch is particles about the string, as a lens sees them: a fine core in focus on a
 * narrow ribbon that turns about the string, a soft layer about it, and a few large out-of-focus
 * discs. Their colours are the ring the agent's pause button wears in that state (console.css
 * `--ag-rim`), running along the stretch while its glints pass the other way. Paused gathers into
 * beads, stopped into dashes, offline into still points. At each gap both ends reach for each
 * other, never touching.
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

/** The rule's box is HEIGHT tall, the string's rest at Y: room above and below for what floats about it. */
export const HEIGHT = 28;
export const Y = 14;
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

// ---- The string ---------------------------------------------------------------------------------

/** Points a second every wave travels the string. */
const SPEED = 32;
/** The string's waves: the seconds one takes to pass (each a whole number of times in 8 s), which way it goes, how big it is undamped, and how hard damping takes it (the finer, the harder). */
const WAVES: readonly (readonly [period: number, dir: number, amp: number, damp: number])[] = [
  [8 / 3, 1, 0.9, 1],
  [2, -1, 0.75, 1.3],
  [4 / 3, 1, 0.55, 1.8],
  [8 / 9, -1, 0.35, 2.4],
  [8 / 13, 1, 0.2, 3.2],
];
/** How free each state leaves the string: 1 undamped, 0 still. */
export const LIVE: Record<Shown, number> = { working: 1, ready: 0.75, needs: 0.62, paused: 0.42, finished: 0.3, crashed: 0.15, offline: 0 };
/** Each line state's ribbon, its half width about the string. */
const HALF: Partial<Record<Shown, number>> = { working: 3, ready: 2.4, needs: 2.1, finished: 1.4 };
/** Half a point between the string's samples. */
const STEP = 0.5;

/** The string at one moment: its height above its rest at x, and the ribbon's half width there. */
export type Strung = { at: (x: number) => number; half: (x: number) => number };

/** The string across `w` at `t` (seconds), held at the rule's ends, each stretch damping it in its region and the damping passing smoothly across each gap's middle (up to 13 points either side). */
export function stringOf(parts: readonly Part[], w: number, t: number): Strung {
  const n = Math.max(2, Math.ceil(w / STEP) + 2);
  const heights = new Float64Array(n);
  const halves = new Float64Array(n);
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
    const x = k * STEP;
    const free = across(x, (p) => LIVE[p.stand]);
    let y = 0;
    if (free > 0) for (const [period, dir, amp, damp] of WAVES) y += amp * free ** damp * Math.sin(TAU * (x / (SPEED * period) - (dir * t) / period) + period * 2.1);
    heights[k] = smooth(Math.min(x, w - x) / 14) * y;
    halves[k] = across(x, (p) => HALF[p.stand] ?? 0);
  }
  const read = (table: Float64Array) => (x: number) => {
    const f = Math.min(n - 1, Math.max(0, x / STEP));
    const k = Math.floor(f);
    return (table[k] ?? 0) + ((table[Math.min(n - 1, k + 1)] ?? 0) - (table[k] ?? 0)) * (f - k);
  };
  return { at: read(heights), half: read(halves) };
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
function glintAt(stand: Shown, s: number, t: number) {
  const { o, rot } = shimmer(stand, t);
  const c = frac(s / CYCLE);
  let best = 0;
  for (const g of GLINTS) {
    const d0 = Math.abs(c - frac(g + rot));
    best = Math.max(best, 1 - Math.min(d0, 1 - d0) / 0.03);
  }
  return smooth(best) * o;
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
 * out of focus); how far from the string the out-of-focus ones float; how far and how fast each
 * stirs on its own (two frequencies, a second); how fast they drift along (points a second, the
 * nearer the faster); the share of particles that now and then flash; and its light. Paused
 * gathers into beads (every so many points, so many particles each), stopped into dashes (on, off),
 * offline into still points.
 */
type Motion = {
  core: number;
  mid: number;
  bokeh: number;
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
  working: { core: 6, mid: 1.4, bokeh: 0.2, spread: 6.5, amp: 0.5, freq: [0.9, 1.6], flow: 9, sparkle: 0.05, bright: 1 },
  ready: { core: 5.5, mid: 1.1, bokeh: 0.15, spread: 5, amp: 0.4, freq: [0.6, 1.1], flow: 5, sparkle: 0.03, bright: 0.88 },
  needs: { core: 5.5, mid: 0.9, bokeh: 0.12, spread: 4.4, amp: 0.34, freq: [0.5, 0.9], flow: 3.2, sparkle: 0.025, bright: 0.94, pulse: true },
  paused: { core: 0, mid: 0, bokeh: 0, spread: 1.5, amp: 0.3, freq: [0.35, 0.6], flow: 0, sparkle: 0.015, bright: 0.85, beads: [6, 12] },
  finished: { core: 5, mid: 0.6, bokeh: 0.06, spread: 3.2, amp: 0.2, freq: [0.25, 0.45], flow: 1.2, sparkle: 0.012, bright: 0.8 },
  crashed: { core: 3.8, mid: 0, bokeh: 0, spread: 0.7, amp: 0.1, freq: [0.15, 0.3], flow: 0, sparkle: 0.006, bright: 0.7, dashes: [3.6, 2.6] },
  offline: { core: 0, mid: 0, bokeh: 0, spread: 0, amp: 0, freq: [0.1, 0.1], flow: 0, sparkle: 0, bright: 0.6, every: 6 },
};
const SEED: Record<Shown, number> = { working: 11, ready: 23, needs: 37, paused: 41, finished: 53, crashed: 67, offline: 71 };
const hash = (n: number) => {
  let x = Math.imul((n | 0) ^ 0x9e3779b9, 0x85ebca6b);
  x ^= x >>> 13;
  x = Math.imul(x, 0xc2b2ae35);
  x ^= x >>> 16;
  return (x >>> 0) / 4294967296;
};
const seedOf = (stand: Shown, k: number) => SEED[stand] * 7919 + k * 131;
/** Where along a stretch its ring starts, so each state's colours lie their own way. */
const offsetOf = (stand: Shown) => SEED[stand] * 37;
/** Needs you brightens and dims, so it's seen. */
export const calling = (t: number) => 0.8 + 0.2 * (0.5 - 0.5 * Math.cos((TAU * t) / 1.6));

/**
 * A frame: its crisp particles in focus (x, y, radius, opacity, colour; five numbers each) and its
 * soft light (x, y, diameter, opacity, colour, how much wider than tall; six each), added over
 * them as light adds. Its arrays are kept from frame to frame.
 */
export type Frame = { dots: number[]; soft: number[] };
export const newFrame = (): Frame => ({ dots: [], soft: [] });

/** A particle's light: its colour from its ring where it is, how bright (it twinkles; a glint or a flash lifts it), how big. */
const lit = { c: 0, r: 0, a: 0, flash: 0 };
function light(p: Part, s0: number, x: number, t: number, b: number, fade: number, pulse: number) {
  const m = MOTION[p.stand];
  const s = x - p.x0 + offsetOf(p.stand);
  const g = glintAt(p.stand, s, t);
  const flash = b === 0 && hash(s0 + 30) < m.sparkle ? Math.max(0, Math.sin(TAU * (t * (0.14 + 0.3 * hash(s0 + 31)) + hash(s0 + 32)))) ** 28 : 0;
  const twinkle = 1 - 0.3 * (0.5 + 0.5 * Math.sin(TAU * (t * (0.35 + 1.1 * hash(s0 + 6)) + hash(s0 + 7))));
  let c = colourAt(p.stand, s, t, (hash(s0 + 8) - 0.5) * 0.08, 0.6 * g);
  if (flash > 0) c = mix(c, RIM[p.stand].glint[0], RIM[p.stand].glint[1], RIM[p.stand].glint[2], flash);
  lit.c = c;
  lit.r = (0.26 + 0.38 * hash(s0 + 1) ** 1.6 + (hash(s0 + 9) < 0.05 ? 0.3 : 0)) * (1 + 0.4 * g + 0.9 * flash);
  lit.a = Math.min(1, m.bright * (0.68 + 0.32 * hash(s0 + 10)) * twinkle + 0.3 * g + 0.6 * flash) * fade * pulse;
  lit.flash = flash;
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

/** A line state's stretch: its core on the ribbon, its soft and out-of-focus layers floating about the string, and the sheen where each glint is. */
function line(f: Frame, p: Part, t: number, S: Strung) {
  const m = MOTION[p.stand];
  const pulse = m.pulse ? calling(t) : 1;
  const L = p.x1 - p.x0;
  const ends = Math.min(10, L / 2.4);
  const layers = [m.core, m.mid, m.bokeh] as const;
  for (let layer = 0; layer < 3; layer++) {
    const n = Math.round(L * (layers[layer] ?? 0));
    const drift = m.flow * (layer === 0 ? 0.12 : layer === 1 ? 0.25 : 0.4);
    for (let q = 0; q < n; q++) {
      const s0 = seedOf(p.stand, layer * 4096 + q);
      const f1 = m.freq[0] * (0.7 + 0.6 * hash(s0 + 2));
      const f2 = m.freq[1] * (0.7 + 0.6 * hash(s0 + 3));
      const x = wrap(p, p.x0 + hash(s0) * L + drift * (0.6 + 0.8 * hash(s0 + 11)) * t + (layer ? 0 : m.amp * 0.4 * Math.sin(TAU * (f2 * 0.5 * t + hash(s0 + 5)))));
      const tp = taperOf(p, x, ends);
      const fade = smooth(Math.min(x - p.x0, p.x1 - x) / (layer === 2 ? 4 : 1.6));
      const stir = m.amp * (0.62 * Math.sin(TAU * (f1 * t + hash(s0 + 4))) + 0.38 * Math.sin(TAU * (f2 * t + hash(s0 + 5))));
      const sy = S.at(x);
      let y: number;
      let size = 1;
      let glow = 1;
      let b = 0;
      if (layer === 0) {
        // On the ribbon: across it (w), as it turns about the string (one turn in 8 s, all along
        // the rule), its near side larger and brighter.
        const w = hash(s0 + 25) * 2 - 1;
        const th = x * 0.05 - (t * TAU) / 8;
        const depth = w * Math.sin(th);
        y = Y + sy + (w * S.half(x) * Math.cos(th) + stir * 0.75) * tp;
        size = 1 + 0.35 * Math.max(0, depth);
        glow = 0.72 + 0.28 * depth;
      } else {
        // Floating about the string, following it: most of the soft layer near it, the
        // out-of-focus discs anywhere within its spread.
        const lane = (hash(s0 + 21) + hash(s0 + 22) + hash(s0 + 23) - 1.5) / 1.5;
        const off = layer === 1 ? lane * m.spread * 0.5 : (hash(s0 + 21) * 2 - 1) * m.spread;
        y = Y + sy * 0.85 + (off + stir * (1 + layer)) * tp;
        b = layer === 1 ? 0.35 + 0.25 * hash(s0 + 20) : 0.7 + 0.3 * hash(s0 + 20);
      }
      const l = light(p, s0, x, t, b, fade, pulse);
      particle(f, x, y, l.r * size * (layer === 2 ? 1.4 : 1), l.a * glow, l.c, b);
      if (l.flash > 0.05) f.soft.push(x, y, 7 * l.r, 0.5 * l.flash, l.c, 1);
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
/** Paused: soft beads riding the string, each a small cloud of particles turning about its middle. */
function beads(f: Frame, p: Part, t: number, S: Strung) {
  const m = MOTION[p.stand];
  const [every, per] = m.beads ?? [6, 12];
  const n = Math.max(1, Math.floor((p.x1 - p.x0 - 2) / every) + 1);
  const x0 = (p.x0 + p.x1 - (n - 1) * every) / 2;
  for (let bead = 0; bead < n; bead++) {
    const cx = x0 + bead * every;
    const cy = Y + S.at(cx);
    for (let q = 0; q < per; q++) {
      const s0 = seedOf(p.stand, bead * 32 + q);
      const soft = q >= per - 2;
      const rad = q === 0 ? 0.1 : (0.4 + 0.65 * hash(s0 + 11)) * (soft ? 1.4 : 1) * m.spread * 0.7;
      const f1 = m.freq[0] * (0.7 + 0.6 * hash(s0 + 2));
      const f2 = m.freq[1] * (0.7 + 0.6 * hash(s0 + 3));
      const l = light(p, s0, cx, t, soft ? 0.5 : 0, 1, 1);
      particle(f, cx + rad * Math.cos(TAU * (f1 * t + hash(s0 + 4))), cy + rad * Math.sin(TAU * (f2 * t + hash(s0 + 5))), q === 0 ? 0.55 : l.r, q === 0 ? Math.min(1, l.a + 0.2) : l.a, l.c, soft ? 0.5 : 0);
    }
  }
}
/** Stopped: short dashes along the string, each packed with fine particles, barely stirring. */
function dashes(f: Frame, p: Part, t: number, S: Strung) {
  const m = MOTION[p.stand];
  const [on, off] = m.dashes ?? [3.6, 2.6];
  for (let d0 = p.x0 + 0.3, d = 0; d0 < p.x1 - 0.3; d0 += on + off, d++) {
    const len = Math.min(on, p.x1 - 0.3 - d0);
    const n = Math.max(2, Math.round(len * m.core));
    for (let q = 0; q < n; q++) {
      const s0 = seedOf(p.stand, d * 64 + q);
      const x = d0 + ((q + 0.5) * len) / n;
      const l = light(p, s0, x, t, 0, 1, 1);
      particle(f, x, Y + S.at(x) + (hash(s0 + 21) - 0.5) * m.spread + m.amp * Math.sin(TAU * (m.freq[0] * t + hash(s0 + 4))), l.r, l.a, l.c, 0);
    }
  }
}
/** Offline: single particles on the string, far apart, dim and still. */
function points(f: Frame, p: Part, t: number, S: Strung) {
  const every = MOTION[p.stand].every ?? 6;
  const n = Math.max(1, Math.floor((p.x1 - p.x0 - 1) / every) + 1);
  const x0 = (p.x0 + p.x1 - (n - 1) * every) / 2;
  for (let q = 0; q < n; q++) {
    const x = x0 + q * every;
    const s0 = seedOf(p.stand, q);
    particle(f, x, Y + S.at(x), 0.6 + 0.12 * hash(s0 + 1), 0.8, light(p, s0, x, t, 0, 1, 1).c, 0);
  }
}

/** How far each end reaches into gap `j` at `t`: out and back, each gap at its own moment, never past SHORT_OF_MIDDLE short of the gap's middle. */
export const reachOf = (j: number, t: number) => 0.35 + 1.25 * (0.5 - 0.5 * Math.cos(TAU * (t / 2.4 + j * 0.37)));
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

/** The rule across `w` at `t` (seconds) into `f`, as `paint` draws it. */
export function frame(parts: readonly Part[], w: number, t: number, f: Frame = newFrame()): Frame {
  f.dots.length = 0;
  f.soft.length = 0;
  const S = stringOf(parts, w, t);
  for (const p of parts) {
    if (p.x1 - p.x0 <= 0.5) continue;
    const m = MOTION[p.stand];
    if (m.beads) beads(f, p, t, S);
    else if (m.dashes) dashes(f, p, t, S);
    else if (m.every) points(f, p, t, S);
    else line(f, p, t, S);
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
      ctx.moveTo(x + r, y);
      ctx.arc(x, y, r, 0, TAU);
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
