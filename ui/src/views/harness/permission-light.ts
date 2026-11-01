/**
 * The permissions scale's lit track (run-settings.tsx): from the scale's start to its knob, a
 * bramble, its canes strings of crisp particles as Mission control's rule draws its light
 * (stands-light.ts), in the working ring's colours (console.css "active") running along them while
 * the ring's glints pass the other way, as the effort line's particles are.
 *
 * It's the danger the stops climb to: the further along the track (the more the agent may do on its
 * own), the more it has grown and the more snarled it is. At the start two fine canes lie taut;
 * along the track more canes sprout, thicker, writhing ever more erratically, kinked and jagged;
 * prickles hook back along them, denser and longer the further along; tendrils arch off the canes
 * and loop back through the tangle, forking near the end, where a cane now and then lashes and its
 * thorns' tips crackle. Its pace barely quickens: danger grows it rather than speeds it. The canes
 * keep to a close core about the rail, the tendrils curling out of it, nothing further than LIMIT
 * from the rail, so the stops' names under the track stay clear.
 *
 * Trailing it, runners creep past the knob onto the track ahead, the more and the further the
 * higher the stop, reaching and drawing back. The scale's stops are drawn with it: past the knob
 * the grey dots they are (warm where a stop turns checks off), stirring as the runners reach for
 * them, the nearest the most; once it reaches one, a bead of its light.
 *
 * A change of stop grows the canes out to the knob, their prickles and tendrils unfolding as they
 * pass, or shrivels them back, withering at the tips (`growFronts`). `permissionFrame` is pure, so
 * tests/permission-light.test.ts checks it; stands-light's `paint` draws it.
 */
import { Y } from "./effort-light";
import { colourAt, dim, glintAt, glintColour, hash, mix, newFrame, shineOf, smooth, type Frame, type Shine } from "./stands-light";

/** How far from the rail anything is drawn (points): the stops' names start 12 under it, their letters about 16. */
export const LIMIT = 12.5;

const TAU = 2 * Math.PI;
const clamp = (v: number, lo: number, hi: number) => Math.min(hi, Math.max(lo, v));
/** From `calm` to `wild` by `k`. */
const lerp = (k: number, calm: number, wild: number) => calm + (wild - calm) * k;

/** The canes, and the runners that creep past the knob. */
const CANES = 9;
const RUNNERS = 9;
/** The scale of a body's waves, kinks, lashes and thorns: the canes' core packs close, the tendrils curling out of it loop wider. */
const CORE = 0.65;
const TENDRIL = 0.8;
/** How far past the core's band about the rail a tendril may reach (points). */
const ROOM = 5.5;
/** Points between a body's samples, and the most samples a body has. */
const DS = 0.9;
const MAX = 4096;
/** Where along the ring's colours the track starts (as the effort line's). */
const OFFSET = 37;
/** How much less each stop past the knob stirs than the one before it, nearer the knob. */
const FALLOFF = 0.6;
/** How fast a cane's front grows out to the knob and shrivels back (points a second, at most; each cane at its own rate). */
const GROW = 240;
const SHRIVEL = 320;
/** The scale's own stop dots (run-settings.css .rs-tick, and [data-warn]'s), their radius in points. */
const STOP: readonly [number, number, number] = [0x3a, 0x3c, 0x44];
const WARN: readonly [number, number, number] = [0x5a, 0x4a, 0x38];
const STOP_R = 2.5;

/** The working ring's glint (stands-light's). */
const GLINT = glintColour("working");
const [GR, GG, GB] = [(GLINT >> 16) & 255, (GLINT >> 8) & 255, GLINT & 255];

/** Randomness read by index (cheaper than hashing each particle). */
const RAND = Float32Array.from({ length: 8192 }, (_, k) => hash(k * 7 + 3));
const rnd = (i: number) => RAND[(i >>> 0) & 8191] ?? 0;

const hash2 = (i: number, j: number) => hash(Math.imul(i, 73856093) ^ Math.imul(j, 19349663));
/** Smooth value noise in two dimensions, −1 to 1. */
function noise2(x: number, y: number) {
  const xi = Math.floor(x);
  const yi = Math.floor(y);
  const xf = x - xi;
  const yf = y - yi;
  const u = xf * xf * (3 - 2 * xf);
  const v = yf * yf * (3 - 2 * yf);
  const a = hash2(xi, yi);
  const b = hash2(xi + 1, yi);
  const c = hash2(xi, yi + 1);
  const d = hash2(xi + 1, yi + 1);
  return 2 * (a + (b - a) * u + (c - a) * v + (a - b - c + d) * u * v) - 1;
}
/** Three octaves of it. */
const fbm = (x: number, y: number) => 0.6 * noise2(x, y) + 0.28 * noise2(2.03 * x + 17.1, 2.01 * y + 3.7) + 0.12 * noise2(4.1 * x + 41.3, 3.97 * y + 9.2);

/** A body's lot (a cane, a tendril or a runner): its waves and their paces, how thick, how fast it grows, which way it curls withering, its lashes, and as a runner how far it reaches, which way it fans and how it breathes. A cane sprouts `start` along the track. */
type Lot = {
  p1: number;
  p2: number;
  w1: number;
  w2: number;
  wrap: number;
  spin: number;
  thick: number;
  rate: number;
  lean: number;
  seed: number;
  key: number;
  noise: number;
  lashEvery: number;
  lashAt: number;
  reach: number;
  fan: number;
  breathe: number;
  start: number;
};
function lotOf(seed: number, v: number, start = 0): Lot {
  const h = (k: number) => hash(seed + k * 17);
  return {
    p1: h(2) * TAU,
    p2: h(3) * TAU,
    w1: 0.8 + 0.12 * v + 0.4 * h(4),
    w2: 1.3 + 0.15 * v + 0.5 * h(5),
    wrap: h(6) * TAU,
    spin: (0.45 + 0.1 * v) * (h(7) < 0.5 ? -1 : 1),
    thick: 0.85 + 0.4 * h(8),
    rate: 0.8 + 0.4 * h(9),
    lean: h(10) < 0.5 ? -1 : 1,
    seed: (seed % 997) * 0.731 + 3.1,
    key: seed * 7919,
    noise: 0.6 + 0.6 * h(11),
    lashEvery: 1.8 + 1.6 * h(12),
    lashAt: h(13) * 5,
    reach: 0.5 + 0.8 * h(14),
    fan: h(15) - 0.5,
    breathe: 0.25 + 0.3 * h(16),
    start,
  };
}
/** The canes: two from the start, the rest sprouting further and further along (never past 85% of the track). */
const CANE_LOTS = Array.from({ length: CANES }, (_, v) => lotOf(v * 131 + 9, v, v < 2 ? 0 : Math.min(0.85, (0.06 + (0.76 * (v - 2)) / (CANES - 3)) * (0.85 + 0.3 * hash(v * 131 + 26)))));
const RUNNER_LOTS = Array.from({ length: RUNNERS }, (_, r) => lotOf(r * 211 + 5003, 2 + r * 0.5));
/** Tendrils' lots by their seeds, the same each frame. */
const TENDRIL_LOTS = new Map<number, Lot>();
function tendrilLot(seed: number, v: number) {
  let l = TENDRIL_LOTS.get(seed);
  if (!l) {
    l = lotOf(seed, v);
    TENDRIL_LOTS.set(seed, l);
  }
  return l;
}

/** The ring's colour at `s` along the track at `t`, a particle's own `grain` leaning it round the ring, `g` a passing glint lifting it. */
const colourOf = (s: number, t: number, grain: number, g: number) => colourAt("working", s + OFFSET, t, grain, 0.6 * g);

// ---- Growth -------------------------------------------------------------------------------------

/** Where each cane's front stands at rest with the knob `fill` along a track `track` long: at the knob, or where it sprouts if that's further. */
export function restFronts(fill: number, track: number) {
  return CANE_LOTS.map((l) => Math.max(l.start * track, fill));
}

/** Moves each cane's front on `dt` seconds toward the knob `fill`: growing out to it, or shrivelling back, each at its own rate, slowing as it arrives. */
export function growFronts(fronts: number[], fill: number, track: number, dt: number) {
  for (const [v, l] of CANE_LOTS.entries()) {
    const from = l.start * track;
    const want = Math.max(from, fill);
    const was = fronts[v] ?? want;
    const now = was + clamp((want - was) * 4.5, -SHRIVEL, GROW) * l.rate * dt;
    fronts[v] = Math.abs(want - now) < 0.2 ? want : now;
  }
}

// ---- The bramble's axis -------------------------------------------------------------------------

/** The axis it grows about: it bends a little, more further along. Worked out every 2 points along the track once a frame, read by every body. */
let AX = new Float32Array(0);
let axN = 0;
function axes(track: number, t: number) {
  const n = Math.ceil(track / 2) + 2;
  if (AX.length < n) AX = new Float32Array(n);
  for (let i = 0; i < n; i++) {
    const x = i * 2;
    const d = Math.min(1, x / track);
    AX[i] = (0.1 + 1.6 * d ** 1.5) * lerp(d, fbm(x * 0.012 + 1.3, t * 0.18 + 3.1), fbm(x * 0.012 + 9.7, t * 0.24 + 5.4)) * smooth(x / 10);
  }
  axN = n;
}
function axisAt(x: number) {
  const f = Math.min(axN - 1.001, Math.max(0, x / 2));
  const i = Math.floor(f);
  const a = AX[i] ?? 0;
  return a + ((AX[i + 1] ?? a) - a) * (f - i);
}

// ---- Bodies -------------------------------------------------------------------------------------

/** A body laid out: each sample's place, how far along the body (`s`), how far along the track (`d`, 0 to 1), which way its twist faces (`p`, −1 to 1), and how withered (`w`). */
type Body = { x: Float32Array; y: Float32Array; s: Float32Array; d: Float32Array; p: Float32Array; w: Float32Array; m: number };
const body = (): Body => ({ x: new Float32Array(MAX), y: new Float32Array(MAX), s: new Float32Array(MAX), d: new Float32Array(MAX), p: new Float32Array(MAX), w: new Float32Array(MAX), m: 0 });
/** The canes, the tendrils, and the tendrils' forks, laid out one at a time (and the runners in B too). */
const A = body();
const B = body();
const C = body();

/** What the bodies of a cane are laid out in: the track, the moment, the knob, the cane's front, and whether it's shrivelling back to the knob. */
type Growing = { track: number; t: number; fill: number; front: number; shrinking: boolean };
/** A strike running back from a cane's tip: how hard, which way, and where it is. */
type Lash = { by: number; side: number; at: number };
/** How a body is laid out: pulled to the rail by `pull`; `loops`, it may turn right back (a tendril snarling), else it always heads on; its `size` (CORE, TENDRIL); `room`, how far past the core's band it may reach. */
type Habit = { pull: number; loops: boolean; lash: Lash | null; size: number; room: number };

/**
 * Lays a body out into `b`: from `x` along the track, `y` from the rail, setting off at `head0`
 * (which fades into its writhing over its first stretch), until it passes `stopX` or is `len` long.
 * Its heading writhes as `l`'s waves say (the calm first stop's beat and the wild last one's, mixed
 * by how far along it is, so its pace barely quickens and never runs away), ever more erratic and
 * jagged further along; it keeps close about the rail. Shrivelling, its stretch past the knob
 * withers, its tip curling up.
 */
function lay(b: Body, l: Lot, x0: number, y0: number, head0: number, stopX: number, len: number, g: Growing, h: Habit) {
  const { track, t, fill, front, shrinking } = g;
  const { pull, loops, lash, size, room } = h;
  let x = x0;
  let y = y0;
  let s = 0;
  let ph1 = l.p1;
  let ph2 = l.p2;
  let m = 0;
  while (m < MAX) {
    const d = clamp(x / track, 0, 1);
    const ax = axisAt(x);
    let w1 = lerp(d, Math.sin(ph1 - l.w1 * t), Math.sin(ph1 - 1.35 * l.w1 * t));
    w1 = Math.sign(w1) * Math.abs(w1) ** (1 - 0.6 * d);
    const w2 = lerp(d, Math.sin(ph2 + l.w2 * t), Math.sin(ph2 + 1.3 * l.w2 * t));
    const erratic = (0.04 + d ** 1.6) * noise2((s * (0.03 + 0.05 * d)) / size + l.seed, t * 0.35 * l.noise);
    // Jags: further along, its heading jumps every so often, held a stretch, then turns sharply.
    const jag = 1.15 * d * d * (hash(l.key + Math.floor(s / ((14 - 8 * d) * size))) - 0.5) * 2;
    const hiss = d > 0.45 ? 0.1 * d ** 3 * noise2((s * 0.5) / size + l.seed + 40, t * 3) : 0;
    const strike = lash ? lash.by * lash.side * Math.exp(-(((x - lash.at) / (8 * size)) ** 2)) : 0;
    let head = ((0.05 + 0.48 * d ** 1.2) * w1 + (0.03 + 0.28 * d ** 1.4) * w2 + erratic + jag + hiss + strike) * smooth(s / (10 * size));
    head += head0 * (1 - smooth(s / (16 * size)));
    const beyond = shrinking ? smooth((x - fill) / 12) : 0;
    const wither = beyond * smooth((x - front + 40) / 40);
    head += wither * l.lean * 1.1;
    // It keeps close about the rail: pulled to it, and past its band there (wider further along)
    // turned back, the harder the further out.
    const dy = y - ax;
    const band = Math.min(LIMIT - 1.5, 1.2 + 4 * d + room);
    head -= (pull / size) * dy + Math.sign(dy) * Math.min(1.2, 0.6 * Math.max(0, Math.abs(dy) - band));
    b.x[m] = x;
    b.y[m] = Y + y;
    b.s[m] = s;
    b.d[m] = d;
    b.w[m] = wither + 0.3 * beyond;
    const turn = l.wrap + (s * (0.06 + 0.05 * d)) / size;
    b.p[m] = lerp(d, Math.cos(turn - l.spin * t), Math.cos(turn - 1.3 * l.spin * t));
    m++;
    if (x >= stopX || s >= len) break;
    ph1 += (TAU / ((55 - 28 * d) * size)) * DS;
    ph2 += (TAU / ((19 - 8 * d) * size)) * DS;
    x += (loops ? Math.cos(head) : Math.max(0.4, Math.cos(head))) * DS;
    y = clamp(clamp(y + Math.sin(head) * DS, -LIMIT + 0.5, LIMIT - 0.5), ax - band - 1, ax + band + 1);
    s += DS;
  }
  b.m = m;
}

const ALL = () => 1;

/** A body's particles: crisp points along it in the ring's colours where they are, each in its own tone, a glint lifting them as it passes, a little soft light; `every` a sample, `thin` their size; `fade(q)`, how much shows at sample q. */
function points(f: Frame, b: Body, l: Lot, t: number, sh: Shine, every: number, thin: number, fade: (q: number) => number) {
  const end = b.s[b.m - 1] ?? 0;
  const key = l.key | 0;
  let i = 0;
  for (let q = 0; q < b.m - 1; q++) {
    const x = b.x[q] ?? 0;
    const y = b.y[q] ?? Y;
    const s = b.s[q] ?? 0;
    const d = b.d[q] ?? 0;
    const near = 0.5 + 0.5 * (b.p[q] ?? 0);
    const tip = smooth((end - s) / 6) * smooth(s / 4);
    const show = (1 - (b.w[q] ?? 0)) * tip * fade(q);
    if (show <= 0.02) {
      i += every;
      continue;
    }
    const g = glintAt(sh, x + OFFSET) * (0.5 + 0.5 * d);
    const c0 = colourOf(x, t, (rnd(key + q * 3) - 0.5) * 0.24, g);
    const lit = (0.45 + 0.45 * near) * (0.6 + 0.4 * d);
    const r0 = (0.2 + 0.3 * d) * l.thick * thin * (0.7 + 0.4 * near) * (0.4 + 0.6 * tip) * (1 + 0.5 * g);
    const dx = (b.x[q + 1] ?? x) - x;
    const dy = (b.y[q + 1] ?? y) - y;
    for (let u = 0; u < every; u++, i++) {
      const px = x + (dx * u) / every;
      const py = y + (dy * u) / every;
      const tone = rnd(key + i * 5 + 1);
      let c = tone < 0.14 ? mix(c0, GR, GG, GB, 0.35) : tone > 0.84 ? dim(c0, 0.76) : c0;
      if (g > 0.05) c = mix(c, GR, GG, GB, Math.min(0.7, g));
      const twinkle = 1 - 0.3 * (0.5 + 0.5 * Math.sin(t * (1 + 2 * rnd(key + i * 5 + 2)) + i));
      const a = Math.min(1, (lit * twinkle + 0.4 * g) * show);
      if (a > 0.02) f.dots.push(px, py, r0 * (0.8 + 0.4 * rnd(key + i * 5 + 3)), a, c);
      if (i % 16 === 0 && show > 0.1) f.soft.push(px, py, 2 + 2.5 * d, (0.05 + 0.1 * d + 0.3 * g) * show, c, 1);
    }
  }
}

/** Prickles along a body: sharp, hooked back, denser and longer further along (`dense`, `size` scale them, `scale` its body's); each opens as the cane's `front` passes and folds back as it shrivels; one heading out from the rail is shorter, its tip within LIMIT; near the end their tips crackle. */
function prickles(f: Frame, b: Body, l: Lot, t: number, sh: Shine, front: number, dense: number, size: number, fade: (q: number) => number, scale: number) {
  let q = 2 + Math.round((3 * hash(l.key + 1)) / DS);
  for (let c = 0; q < b.m - 2; c++) {
    const d = b.d[q] ?? 0;
    const seed = l.key + c * 23;
    const x0 = b.x[q] ?? 0;
    const y0 = b.y[q] ?? Y;
    const open = smooth((front - x0) / 12) * (1 - (b.w[q] ?? 0)) * smooth((b.s[q] ?? 0) / 5) * fade(q);
    if (open > 0.05 && hash(seed) < (0.18 + 0.82 * d ** 1.1) * dense) {
      const side = hash(seed + 1) < 0.5 ? -1 : 1;
      const tangent = Math.atan2((b.y[q + 1] ?? y0) - y0, (b.x[q + 1] ?? x0) - x0);
      let dir = tangent + Math.PI - side * (0.55 + 0.6 * hash(seed + 3));
      const out = Math.sin(dir) * Math.sign(y0 - Y);
      let len = (1.2 + 5.2 * d ** 1.2) * scale * (0.5 + 0.7 * hash(seed + 2)) * open * size;
      if (out > 0) len = Math.min(len, Math.max(0, (LIMIT - Math.abs(y0 - Y)) / out));
      const base = (0.2 + 0.3 * d) * l.thick * 1.05 * size;
      const flash = d > 0.65 ? Math.max(0, Math.sin(t * (2.5 + 3 * hash(seed + 4)) + seed)) ** 40 * smooth((d - 0.65) / 0.25) : 0;
      const c0 = colourOf(x0, t, 0, glintAt(sh, x0 + OFFSET));
      let px = x0;
      let py = y0;
      for (let k = 0; k < 4; k++) {
        if (k > 0) {
          dir += side * 0.2;
          px += (Math.cos(dir) * len) / 3;
          py = clamp(py + (Math.sin(dir) * len) / 3, Y - LIMIT, Y + LIMIT);
        }
        const fk = k / 3;
        f.dots.push(px, py, base * (1 - 0.8 * fk) + 0.1, Math.min(1, (0.75 + 0.2 * d) * open + flash * fk), fk > 0.6 && flash > 0.05 ? mix(c0, GR, GG, GB, flash) : c0);
      }
      if (flash > 0.05) f.soft.push(px, py, 4, 0.6 * flash, mix(c0, GR, GG, GB, 0.5), 1);
    }
    q += Math.max(1, Math.round(((15 - 10 * d) * (0.6 + 0.8 * hash(seed + 5))) / (DS * dense)));
  }
}

// ---- The frame ----------------------------------------------------------------------------------

/** What else a frame is drawn with: the scale's `stops` (points along the track, the last at its end), which of them turn checks off (`warn`), where each cane's front stands (`growFronts`; at rest if not given), and the frame to draw `into`. */
export type Scene = { stops?: readonly number[]; warn?: readonly boolean[]; fronts?: readonly number[]; into?: Frame };

/**
 * The bramble at `t` (seconds), its knob `fill` points along the track, as stands-light's `paint`
 * draws it. Each stretch of the track is as grown and snarled as it is far along, so what's lit
 * already stays as it was: a change of stop grows the canes out to the knob or shrivels them back.
 */
export function permissionFrame(fill: number, t: number, scene: Scene = {}): Frame {
  const { stops = [], warn = [], into: f = newFrame() } = scene;
  f.dots.length = 0;
  f.soft.length = 0;
  const track = stops.length > 1 ? (stops[stops.length - 1] ?? 0) : Math.max(1, fill);
  if (track < 1) return f;
  const fronts = scene.fronts ?? restFronts(fill, track);
  const level = clamp(fill / track, 0, 1);
  axes(track, t);
  const sh = shineOf("working", t);
  const g: Growing = { track, t, fill, front: 0, shrinking: false };
  for (const [v, l] of CANE_LOTS.entries()) {
    const from = l.start * track;
    const front = fronts[v] ?? from;
    if (front - from < 0.5) continue;
    g.front = front;
    g.shrinking = front > fill + 0.5;
    // Now and then, the further along it reaches the likelier, a cane lashes: a strike runs back from its tip.
    const reach = clamp(front / track, 0, 1);
    const k = Math.floor((t + l.lashAt) / l.lashEvery);
    const since = t + l.lashAt - k * l.lashEvery;
    const lashing = hash(l.key + k) < smooth((reach - 0.5) / 0.4) * 0.8 && since < 0.9;
    const lash = lashing ? { by: (0.55 + 0.45 * hash(l.key + k + 2)) * Math.exp(-since / 0.32) * smooth(since / 0.05), side: hash(l.key + k + 1) < 0.5 ? -1 : 1, at: front - 200 * since } : null;
    lay(A, l, from, 0, 0, front, Infinity, g, { pull: lerp(reach, 0.08, 0.05), loops: false, lash, size: CORE, room: 0 });
    points(f, A, l, t, sh, 1, 1, ALL);
    prickles(f, A, l, t, sh, front, 1, 1, ALL, CORE);
    // Tendrils, further along only, the more and longer the further: each sprouts as the cane
    // grows past, arches off it and may loop right back through the tangle; near the end some fork.
    let q = Math.round((8 + 14 * hash(l.key + 5)) / DS);
    for (let c = 0; q < A.m - 2; c++) {
      const d = A.d[q] ?? 0;
      const seed = l.key + 501 + c * 37;
      const x0 = A.x[q] ?? 0;
      const y0 = A.y[q] ?? Y;
      const open = smooth((front - x0) / 16) * (1 - (A.w[q] ?? 0));
      if (open > 0.05 && hash(seed) < smooth((d - 0.22) / 0.35) * 0.62) {
        const side = hash(seed + 1) < 0.5 ? -1 : 1;
        const tangent = Math.atan2((A.y[q + 1] ?? y0) - y0, (A.x[q + 1] ?? x0) - x0);
        const sub = tendrilLot(seed, 3);
        const len = (8 + 30 * d ** 1.3) * TENDRIL * (0.55 + 0.6 * hash(seed + 2)) * open;
        lay(B, sub, x0, y0 - Y, tangent + side * (0.6 + 0.5 * hash(seed + 3)), Infinity, len, g, { pull: 0.02, loops: d > 0.45, lash: null, size: TENDRIL, room: ROOM });
        points(f, B, sub, t, sh, 1, 0.85, ALL);
        prickles(f, B, sub, t, sh, Infinity, 0.75, 0.8, ALL, TENDRIL);
        if (d > 0.7 && hash(seed + 9) < smooth((d - 0.7) / 0.25) * 0.45 && B.m > 8) {
          const j = Math.floor(B.m * (0.4 + 0.3 * hash(seed + 10)));
          const xj = B.x[j] ?? 0;
          const yj = B.y[j] ?? Y;
          const twig = tendrilLot(seed + 77, 4);
          const tan = Math.atan2((B.y[j + 1] ?? yj) - yj, (B.x[j + 1] ?? xj) - xj);
          lay(C, twig, xj, yj - Y, tan - side * (0.7 + 0.4 * hash(seed + 11)), Infinity, len * 0.5, g, { pull: 0.02, loops: true, lash: null, size: TENDRIL, room: ROOM });
          points(f, C, twig, t, sh, 1, 0.7, ALL);
          prickles(f, C, twig, t, sh, Infinity, 0.75, 0.65, ALL, TENDRIL);
        }
      }
      q += Math.max(2, Math.round(((36 - 20 * d) * (0.7 + 0.6 * hash(seed + 4))) / DS));
    }
  }
  runners(f, fronts, fill, track, level, t, sh);
  stopsOf(f, stops, warn, fill, track, level, t, sh);
  return f;
}

/** Past the knob it creeps on onto the track ahead once its canes reach the knob: the more runners and the further the higher the stop, each reaching and drawing back, thinning away from the knob, never past the track's end. */
function runners(f: Frame, fronts: readonly number[], fill: number, track: number, level: number, t: number, sh: Shine) {
  const reached = clamp(Math.max(fronts[0] ?? 0, fronts[1] ?? 0), 0, track);
  const ready = smooth((reached - fill + 25) / 25);
  const room = track - fill;
  const many = (1 + 8 * level ** 1.1) * ready;
  if (room <= 6 || many <= 0.05) return;
  const g: Growing = { track, t, fill, front: fill, shrinking: false };
  const y0 = axisAt(fill);
  for (const [r, l] of RUNNER_LOTS.entries()) {
    const show = clamp(many - r, 0, 1);
    if (show <= 0) break;
    const reach = (track / 5) * (0.18 + 0.75 * level ** 1.1) * l.reach * (0.82 + 0.18 * Math.sin(t * l.breathe + l.p1));
    lay(B, l, fill, y0, l.fan * (0.5 + 0.6 * level), track, Math.min(reach, room + 20), g, { pull: 0.04, loops: false, lash: null, size: TENDRIL, room: 2 });
    const away = (q: number) => {
      const x = B.x[q] ?? fill;
      return show * Math.exp(-(x - fill) / Math.max(4, 0.7 * reach)) * smooth((track - x) / 6);
    };
    points(f, B, l, t, sh, 1, 0.85, away);
    prickles(f, B, l, t, sh, Infinity, 0.7, 0.7, away, TENDRIL);
  }
}

/** The stops: each it has reached a bead of its light, breathing, larger and brighter the further along; past the knob the grey dots they are (warm where `warn`), stirring as the runners reach for them, the nearest the most and each further one FALLOFF as much. */
function stopsOf(f: Frame, stops: readonly number[], warn: readonly boolean[], fill: number, track: number, level: number, t: number, sh: Shine) {
  const gap = stops.length > 1 ? Math.max(1e-6, (track - (stops[0] ?? 0)) / (stops.length - 1)) : track;
  for (const [i, x] of stops.entries()) {
    const d = clamp(x / track, 0, 1);
    const on = fill < 1 ? 0 : smooth((fill + 3 - x) / 6);
    if (on > 0) {
      const y = Y + axisAt(Math.min(x, fill));
      const breath = 0.5 + 0.5 * lerp(d, Math.sin(t * 1.2 + i * 1.7), Math.sin(t * 1.7 + i * 1.7));
      const gl = glintAt(sh, x + OFFSET) * (0.5 + 0.5 * d);
      const c = mix(colourOf(x, t, 0, gl), GR, GG, GB, 0.35);
      f.dots.push(x, y, (1.4 + 0.45 * d) * (1 + 0.1 * breath + 0.25 * gl), (0.85 + 0.15 * d) * on, c);
      f.soft.push(x, y, (5 + 8 * d) * (0.85 + 0.3 * breath), ((0.2 + 0.3 * d) * (0.6 + 0.4 * breath) + 0.3 * gl) * on, c, 1);
    }
    if (on < 1) {
      const near = FALLOFF ** Math.max(0, (x - fill) / gap - 1) * (0.85 + 0.15 * level);
      const p = hash(6211 + 89 * i);
      const stir = lerp(near, Math.sin(TAU * (0.18 * t + p)), Math.sin(TAU * (0.3 * t + 1.3 * p)));
      const breath = 0.5 + 0.5 * stir;
      const c = colourOf(x, t, 0, 0);
      const [r, gg, b] = warn[i] ? WARN : STOP;
      const grey = mix((r << 16) | (gg << 8) | b, (c >> 16) & 255, (c >> 8) & 255, c & 255, near * (0.15 + 0.4 * breath));
      const y = Y + 0.6 * near * stir;
      f.dots.push(x, y, STOP_R * (1 + near * (0.04 + 0.16 * breath)), 1 - on, grey);
      f.soft.push(x, y, 8 + 6 * near * breath, near * (0.06 + 0.22 * breath) * (1 - on), c, 1);
    }
  }
}
