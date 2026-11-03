/**
 * A compaction under way (signals.tsx `Condense`): the conversation's line drawing itself together.
 * A close thread of fine grains lies along the line in the prism the summary's bar will wear once
 * it's done (signals.css `--sg-prism`), laid across the conversation's width, a slow swell of light
 * running along it while the work goes on. Finer grains scattered either side drift in to it,
 * slowing as they come and fading into it as they arrive, each starting again elsewhere; the line
 * itself barely breathes. Nothing flashes and nothing turns: Mission control's rule
 * (stands-light.ts) is a thread that flows, the effort scale (effort-light.ts) a swarm, and this
 * one gathers.
 *
 * `condenseFrame` is pure, so tests/condense-light.test.ts checks it; stands-light's `paint` draws it.
 */
import { dim, hash, mix, newFrame, smooth, wander, type Frame } from "./stands-light";

/** The canvas's height (points) and the line's rest in it: room either side for the grains coming in. */
export const HEIGHT = 44;
export const Y = 22;

const TAU = 2 * Math.PI;
const frac = (v: number) => v - Math.floor(v);

/** The site's prism (signals.css `--sg-prism`), from the line's start to its end. */
const PRISM: readonly (readonly [number, number, number, number])[] = [
  [0, 0xb8, 0xd9, 0xd2],
  [0.28, 0xa6, 0xc4, 0xed],
  [0.53, 0xb8, 0xa6, 0xd5],
  [0.75, 0xd6, 0xa2, 0xaa],
  [1, 0xd8, 0xcb, 0xb0],
];
const pack = (r: number, g: number, b: number) => (Math.round(r) << 16) | (Math.round(g) << 8) | Math.round(b);
/** The prism's colour `u` of the way along. */
export function prismAt(u: number) {
  const v = Math.min(1, Math.max(0, u));
  let j = 1;
  while (j < PRISM.length - 1 && v > (PRISM[j]?.[0] ?? 1)) j++;
  const [a, ar, ag, ab] = PRISM[j - 1] ?? [0, 0, 0, 0];
  const [b, br, bg, bb] = PRISM[j] ?? [1, 0, 0, 0];
  const k = b > a ? (v - a) / (b - a) : 0;
  return pack(ar + (br - ar) * k, ag + (bg - ag) * k, ab + (bb - ab) * k);
}

/** The thread: grains a point, how far across the line they lie, how fast they flow along it (points a second), and how bright. */
const THREAD = { grains: 5.5, half: 1.1, flow: 3, bright: 0.95 } as const;
/**
 * The grains coming in: so many a point; the seconds one takes to arrive (a spread, each its own);
 * how far out they start (points, the least and the most: most of them near); and how far along
 * the line one drifts as it comes.
 */
const GATHER = { grains: 2.4, takes: [2.4, 4.2], from: [3, 15], drift: 6 } as const;
/** The swell along the thread: the seconds it takes to cross the line, how wide it is (a share of the line, σ), and how much it lifts the light. */
const SWELL = { cross: 4.8, wide: 0.07, lift: 0.3 } as const;
/** The thread's own breath: two slow waves along it, under half a point. */
const BREATH = 0.45;

/** Where the line is at x along a line `w` long at `t`: a slow breath, held at its two ends. */
export function lineAt(w: number, x: number, t: number) {
  const ends = smooth(Math.min(x, w - x) / 24);
  return BREATH * ends * (0.6 * Math.sin(TAU * (x / 260 - t / 7)) + 0.4 * Math.sin(TAU * (x / 140 + t / 5.3) + 1.3)) * (0.8 + 0.2 * wander(5501, t / 4));
}
/** How much the swell lifts the light at x: one at a time, crossing the line from start to end and starting again. */
const swellAt = (w: number, x: number, t: number) => {
  const u = frac(t / SWELL.cross) * 1.3 - 0.15;
  return SWELL.lift * Math.exp(-(((x / w - u) / SWELL.wide) ** 2) / 2);
};
/** How much is shown near the line's ends: a little room to come in from nothing. */
const endsOf = (w: number, x: number) => smooth(Math.min(x, w - x) / 18);

/** The light a grain gives at `u` along the line: the prism there, lighter or deeper by its `tone`, lifted by `lift`. */
function tone(u: number, k: number, lift: number) {
  const c = prismAt(u);
  const lit = k > 0 ? mix(c, 0xf2, 0xee, 0xf8, 0.28 * k) : dim(c, 1 + 0.2 * k);
  return lift > 0.01 ? mix(lit, 0xf2, 0xee, 0xf8, Math.min(1, 0.7 * lift)) : lit;
}

/** The compaction's line across `w` at `t` (seconds), as stands-light's `paint` draws it. */
export function condenseFrame(w: number, t: number, f: Frame = newFrame()): Frame {
  f.dots.length = 0;
  f.soft.length = 0;
  if (w < 2) return f;
  // The thread: grains flowing along the line, close about it.
  const n = Math.round(w * THREAD.grains);
  for (let q = 0; q < n; q++) {
    const s0 = 70001 + q * 131;
    const x = frac(hash(s0) + ((0.6 + 0.8 * hash(s0 + 11)) * THREAD.flow * t) / w) * w;
    const lane = (hash(s0 + 21) + hash(s0 + 22) + hash(s0 + 23) - 1.5) / 1.5;
    const stir = 0.18 * Math.sin(TAU * ((0.3 + 0.5 * hash(s0 + 2)) * t + hash(s0 + 4)));
    const lift = swellAt(w, x, t);
    const twinkle = 1 - 0.18 * (0.5 + 0.5 * Math.sin(TAU * (t * (0.15 + 0.6 * hash(s0 + 6)) + hash(s0 + 7))));
    const a = Math.min(1, THREAD.bright * (0.45 + 0.5 * hash(s0 + 10) ** 0.8) * twinkle + 0.5 * lift) * endsOf(w, x);
    if (a <= 0.01) continue;
    f.dots.push(x, Y + lineAt(w, x, t) + lane * THREAD.half + stir, (0.26 + 0.5 * hash(s0 + 1) ** 2) * (1 + 0.3 * lift), a, tone(x / w, 2 * hash(s0 + 44) - 1, lift));
  }
  // The grains coming in: each from its own place either side, slowing as it nears the line and
  // fading into it, then again from somewhere else.
  const m = Math.round(w * GATHER.grains);
  const [t0, t1] = GATHER.takes;
  const [near, far] = GATHER.from;
  for (let q = 0; q < m; q++) {
    const s0 = 80021 + q * 137;
    const takes = t0 + (t1 - t0) * hash(s0);
    const life = t / takes + hash(s0 + 1);
    const round = Math.floor(life);
    const u = life - round;
    const r0 = s0 + 7 * round;
    const side = hash(r0 + 3) < 0.5 ? -1 : 1;
    const from = near + (far - near) * hash(r0 + 4) ** 1.6;
    const came = 1 - (1 - u) ** 3;
    const x0 = hash(r0 + 2) * w;
    const x = x0 + GATHER.drift * (hash(r0 + 5) - 0.5) * 2 * came;
    if (x < 0 || x > w) continue;
    const y = Y + lineAt(w, x, t) + side * from * (1 - came);
    // In over its first fifth; out as it reaches the line, into the thread.
    const a = 0.8 * (0.5 + 0.5 * hash(r0 + 6)) * smooth(u / 0.2) * smooth((1 - u) / 0.35) * endsOf(w, x);
    if (a <= 0.01) continue;
    f.dots.push(x, y, 0.2 + 0.32 * hash(r0 + 1) * (1 - 0.4 * came), a, tone(x / w, 2 * hash(r0 + 8) - 1, 0));
  }
  // Under the swell a faint light along the line, so it reads as light passing.
  const at = (frac(t / SWELL.cross) * 1.3 - 0.15) * w;
  if (at > 0 && at < w) f.soft.push(at, Y + lineAt(w, at, t), 8, 0.16 * endsOf(w, at), tone(at / w, 0.5, 1), Math.max(4, (SWELL.wide * w) / 4));
  return f;
}
