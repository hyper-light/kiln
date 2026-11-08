/**
 * The light Mission control's rule (stands-dots.ts), a compaction (condense-light.ts) and the
 * scales' lit tracks (effort-light.ts, permission-light.ts) share: each state's colour (its
 * badge's) and the ring the agent's pause button wears in it (console.css `--ag-rim`), the ring's
 * glints, a grain's tone in its state's colour, smooth noise, and `paint`, which draws a frame of
 * particles on a canvas. It also counts the rule's numbers into their cells (`cells`).
 */
import type { MissionStand } from "./mission-rows";

export type Shown = Exclude<MissionStand, "ended" | "archived">;

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

/** How long a change of counts takes to settle into the rule's new places (ms). */
export const GROW_MS = 600;

const TAU = 2 * Math.PI;
export const smooth = (x: number) => (x <= 0 ? 0 : x >= 1 ? 1 : x * x * (3 - 2 * x));
const frac = (v: number) => v - Math.floor(v);
export const easeOut = (x: number) => 1 - (1 - Math.min(1, Math.max(0, x))) ** 3;

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

export const hash = (n: number) => {
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
export const wander = (seed: number, u: number) => 0.7 * noise(seed, u) + 0.3 * noise(seed + 7777, 2.7 * u + 0.5);

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
export const mix = (c: number, r: number, g: number, b: number, k: number) => pack(((c >> 16) & 255) + (r - ((c >> 16) & 255)) * k, ((c >> 8) & 255) + (g - ((c >> 8) & 255)) * k, (c & 255) + (b - (c & 255)) * k);
export const dim = (c: number, k: number) => pack(((c >> 16) & 255) * k, ((c >> 8) & 255) * k, (c & 255) * k);

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
export type Shine = { o: number; at: readonly number[] };
export const shineOf = (stand: Shown, t: number): Shine => {
  const { o, rot } = shimmer(stand, t);
  return { o, at: GLINTS.map((g) => frac(g + rot)) };
};
export function glintAt(sh: Shine, s: number) {
  const c = frac(s / CYCLE);
  let best = 0;
  for (const g of sh.at) {
    const d0 = Math.abs(c - g);
    best = Math.max(best, 1 - Math.min(d0, 1 - d0) / 0.03);
  }
  return smooth(best) * sh.o;
}
/** The ring's colour at `s` along a stretch at `t`, `grain` a particle's own lean round it, mixed toward the glint by `g`. */
export function colourAt(stand: Shown, s: number, t: number, grain: number, g: number) {
  const c = LUTS[stand][Math.floor(frac(s / CYCLE - t / RIM[stand].current + grain) * 255)] ?? 0;
  const [r, gg, b] = RIM[stand].glint;
  return g > 0 ? mix(c, r, gg, b, Math.min(1, g)) : c;
}
export const glintColour = (stand: Shown) => pack(RIM[stand].glint[0], RIM[stand].glint[1], RIM[stand].glint[2]);

// ---- Grains -------------------------------------------------------------------------------------

/** Needs you brightens and dims, so it's seen. */
export const calling = (t: number) => 0.8 + 0.2 * (0.5 - 0.5 * Math.cos((TAU * t) / 1.6));

/**
 * A frame: its crisp particles (x, y, radius, opacity, colour; five numbers each) and its soft
 * light (x, y, diameter, opacity, colour, how much wider than tall; six each), added over them as
 * light adds, and arrays its maker works in. Its arrays are kept from frame to frame.
 */
export type Frame = { dots: number[]; soft: number[]; scratch: Float64Array[] };
export const newFrame = (): Frame => ({ dots: [], soft: [], scratch: [] });

/** Each state's own colour (its badge's). */
const BASE = Object.fromEntries(LIVELINESS.map((stand) => [stand, rgb(LOOK[stand].colour)])) as Record<Shown, number[]>;
/** Points a turn of a state's ring takes across the rule, and how many times slower than the ring's own its colours pass: long and slow, a tint rather than a band. */
const LONG = 420;
const SLOWER = 3;
/** How much of the ring's colour a grain wears over its state's own. */
const TINT = 0.4;
/**
 * A grain's colour at `s` points across the rule at `t`: its state's own, the ring passing slowly
 * through it (`lean` the grain's own lean round it), lighter toward the ring's glint or deeper by
 * its `tone` (-1 to 1), and lifted toward the glint by `lift` (a dot lit, or flashing as it's born).
 */
export function toneAt(stand: Shown, s: number, t: number, lean: number, tone: number, lift = 0) {
  const R = RIM[stand];
  const ring = LUTS[stand][Math.floor(frac(s / LONG - t / (R.current * SLOWER) + lean) * 255)] ?? 0;
  const [r = 0, g = 0, b = 0] = BASE[stand];
  const [gr, gg, gb] = R.glint;
  let c = mix(ring, r, g, b, 1 - TINT);
  c = tone > 0 ? mix(c, gr, gg, gb, 0.3 * tone) : dim(c, 1 + 0.22 * tone);
  return lift > 0.01 ? mix(c, gr, gg, gb, Math.min(1, 0.6 * lift)) : c;
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
