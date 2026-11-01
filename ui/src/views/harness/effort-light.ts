/**
 * The effort scale's lit line (run-settings.tsx): from the scale's start to its knob, a string of
 * light as Mission control's rule draws one (stands-light.ts), its particles in the working ring's
 * colours (console.css "active") running along it while its glints pass the other way: a fine core
 * on a narrow ribbon turning about the string, a soft layer about it, a few large out-of-focus
 * discs, and dust furthest out, the further out the later following it.
 *
 * It tapers out from the start to the knob: each point of it is as lively as the notch at that
 * point of the track, so at the start it's a fine thread, as calm as the first notch, and towards
 * the knob it has more of all of it: more particles (volume), brighter (intensity), drifting and
 * stirring faster (velocity), and further out about a more troubled string, its waves bigger and
 * longer, its ripples harder, more of its particles flashing (chaos). So each notch up draws it
 * further and wilder at its end: at the second notch a short, quiet thread; at the last it swarms
 * into the knob. The ribbon's turn, the colours and the glints go at the knob's notch's pace, and
 * ripples come as often as it says. A change of notch speeds it all up or slows it down from where
 * it is (its clocks count on at the notch's paces) and sends a ripple down it from the knob: a step
 * up strikes it, a step down lets it settle.
 *
 * Past the knob the line spills on to the track ahead, thinning and widening as it goes: the higher
 * the notch, the fuller it is at the knob, the more of it spills and the further. The scale's stops
 * are drawn with it: past the knob the grey dots they are, stirring as the spill reaches them, the
 * nearer the knob the more; once the line reaches one, a bead of its light on the string,
 * breathing, and livelier the further along it sits. So it all peaks at the knob.
 *
 * `effortFrame` is pure, so tests/effort-light.test.ts checks it; stands-light's `paint` draws it.
 */
import { colourAt, dim, glintAt, glintColour, hash, mix, newFrame, shineOf, smooth, wander, type Frame, type Shine } from "./stands-light";

/** The canvas's height (points) and the line's rest in it: room either side for the swarm. */
export const HEIGHT = 40;
export const Y = 20;

const TAU = 2 * Math.PI;
const clamp = (v: number, lo: number, hi: number) => Math.min(hi, Math.max(lo, v));

/**
 * Everything at `level` (0 the first notch, 1 the last), each from calm to wild. The wildest
 * parts grow late (their curve), so the first notches stay quiet and the last ones run away.
 */
export function energy(level: number) {
  const l = clamp(level, 0, 1);
  const by = (a: number, b: number, curve = 1) => a + (b - a) * l ** curve;
  return {
    // Intensity: how bright the particles and the glints are.
    bright: by(0.5, 1.1),
    glint: by(0.35, 1),
    // Volume: particles a point, layer by layer (the core, the soft layer, out of focus, dust).
    layers: [by(1.6, 8, 1.15), by(0.25, 2.8, 1.3), by(0.02, 0.4, 1.5), by(0.1, 1.2, 1.3)],
    // Velocity: points a second they drift along; how often they stir (two frequencies, a
    // second); and the seconds a turn of the ribbon, of the colours and of the glints takes.
    flow: by(2, 24, 1.2),
    freq: [by(0.3, 1.4), by(0.55, 2.5)],
    turn: by(16, 3.4),
    current: by(14, 3.4),
    spark: by(10, 2.6),
    // Chaos: how far they stir and float; the ribbon's half width; the string's waves (points)
    // and how fast they run along it; the chance of a ripple each half second, how hard one is
    // and how fast it runs; the share of particles that flash.
    amp: by(0.12, 1.2, 1.4),
    spread: by(1.2, 9.5, 1.3),
    half: by(0.5, 3.4, 1.2),
    waves: by(0.12, 2.6, 1.6),
    speed: by(18, 64),
    ripples: by(0.02, 0.5, 1.6),
    hard: by(0.5, 2.6, 1.3),
    medium: by(70, 300),
    sparkle: by(0.005, 0.15, 1.5),
  };
}
export type Energy = ReturnType<typeof energy>;

/** The first notch's and the last's. */
const CALM = energy(0);
const WILD = energy(1);
/** Energy at each of these steps of level, worked out once: the taper reads its nearest step at each point. */
const STEPS = 256;
const TABLE: Energy[] = Array.from({ length: STEPS + 1 }, (_, i) => energy(i / STEPS));
const energyAt = (level: number) => TABLE[Math.round(clamp(level, 0, 1) * STEPS)] ?? CALM;

// ---- The string ---------------------------------------------------------------------------------

/** The string's own waves: the seconds one takes to pass, which way it goes, and how big it is (times `waves`). */
const WAVES: readonly (readonly [period: number, dir: number, amp: number])[] = [
  [8 / 3, 1, 0.9],
  [2, -1, 0.75],
  [4 / 3, 1, 0.55],
  [8 / 9, -1, 0.35],
  [8 / 13, 1, 0.2],
];
/** Its weather: its motion swells and calms, from under half as much to over twice as much. */
const weather = (t: number) => Math.exp(1.1 * wander(9001, t / 6)) / 1.08;
/** However hard it's struck, the string keeps to its box: past the knee it gives less and less. */
const KNEE = 5;
const LIMIT = 11;
const knee = (y: number, from: number, most: number) => {
  const a = Math.abs(y);
  return a <= from ? y : Math.sign(y) * (from + (most - from) * Math.tanh((a - from) / (most - from)));
};
const limit = (y: number) => knee(y, KNEE, LIMIT);
/** However far the swarm is flung, it keeps inside the canvas: past 11 points from the line it gives less and less, never past 18. */
const inBox = (dy: number) => knee(dy, 11, Y - 2);
/** A change of notch: how long ago (seconds) and how hard it moved the string at the knob; a step up strikes it, a step down (`down`) lets it settle. */
export type Kick = { ago: number; size: number; down?: boolean };
/** Half a point between the string's samples; the strings the far particles follow are sampled every four, these seconds behind it. */
const STEP = 0.5;
const LATER_STEP = 4;
const LATER = [0.05, 0.12, 0.22] as const;

type Ripple = { t0: number; x0: number; both: boolean; period: number; crests: number; size: number };
/**
 * The ripples on a line `fill` long at `t`: in each half second, by chance, one comes in at its
 * start or strikes somewhere along it and runs both ways; most soft, a few hard (their size, times
 * how hard the line is where they've come to). Those the notch only just has fade with it, so a
 * change of notch swells or stills them rather than one appearing or vanishing mid-way.
 */
function ripplesAt(t: number, fill: number, e: Energy): Ripple[] {
  const out: Ripple[] = [];
  for (let k = Math.floor((t - 6) / 0.5); k <= Math.floor(t / 0.5); k++) {
    const s0 = 7919 * k + 211;
    const t0 = (k + hash(s0)) * 0.5;
    const had = clamp((e.ripples - hash(s0 + 1)) / (0.2 * e.ripples + 0.02), 0, 1);
    if (t0 > t || had <= 0) continue;
    const both = hash(s0 + 2) >= 0.35;
    const size = had * Math.min(4, 0.5 * (1 - hash(s0 + 6)) ** (-1 / 1.2)) * (hash(s0 + 7) < 0.5 ? -1 : 1);
    out.push({ t0, x0: both ? (0.1 + 0.8 * hash(s0 + 4)) * fill : 0, both, period: 0.25 * 6.4 ** hash(s0 + 3), crests: 1 + 2 * hash(s0 + 5), size });
  }
  return out;
}

/** Arrays the string is worked out in, kept from frame to frame by a frame that asks for them. */
function table(pool: Float64Array[] | undefined, slot: number, n: number) {
  if (!pool) return new Float64Array(n);
  let a = pool[slot];
  if (!a || a.length < n) pool[slot] = a = new Float64Array(Math.ceil(n * 1.25) + 8);
  return a.subarray(0, n);
}
/** Reads samples `step` apart at any x, between them. */
const reader = (samples: Float64Array, step: number) => (x: number) => {
  const n = samples.length;
  const f = clamp(x / step, 0, n - 1);
  const k = Math.floor(f);
  return (samples[k] ?? 0) + ((samples[Math.min(n - 1, k + 1)] ?? 0) - (samples[k] ?? 0)) * (f - k);
};

/** The string's heights along a line `fill` long at `t`, on a track `track` long (each point as lively as the notch there), its knob at `level`, a sample every `step` points, held at its start and at the knob. */
function heights(fill: number, level: number, t: number, kick: Kick | undefined, step: number, into: Float64Array, track: number) {
  const top = energy(level);
  const ripples = ripplesAt(t, fill, top);
  const gust = weather(t);
  const strength = WAVES.map(([period], j) => 1 + 0.6 * wander(1009 * j + 1, t / (3 * period)));
  const phase = WAVES.map(([period], j) => 3.5 * wander(1009 * j + 503, t / (3 * period) + 0.37));
  // Its waves run faster the further along (energy's `speed` grows with the notch), so they
  // lengthen out along it: the seconds they take to reach x, at each point's pace on the way.
  const grow = (WILD.speed - CALM.speed) / track;
  const arrive = (x: number) => (grow > 1e-9 ? Math.log1p((grow * x) / CALM.speed) / grow : x / CALM.speed);
  for (let k = 0; k < into.length; k++) {
    const x = Math.min(fill, k * step);
    const e = energyAt(x / track);
    const reach = arrive(x);
    let y = 0;
    for (let j = 0; j < WAVES.length; j++) {
      const [period, dir, amp] = WAVES[j] ?? [1, 1, 0];
      y += amp * (strength[j] ?? 0) * Math.sin(TAU * (reach / period - (dir * t) / period) + (phase[j] ?? 0));
    }
    y *= e.waves * gust;
    for (const r of ripples) {
      const gone = Math.abs(x - r.x0) / top.medium;
      const s = t - r.t0 - gone;
      if (s <= 0 || s > 3 * r.crests * r.period) continue;
      y += e.hard * r.size * smooth(s / (0.25 * r.period)) * Math.exp(-s / (0.6 * r.crests * r.period) - gone / 1.6) * Math.sin((TAU * s) / r.period);
    }
    // A change of notch's ripple, running back from the knob: a step up lifts it, quicker and
    // livelier; a step down lets it settle, rising slower and swinging longer and softer. The
    // calmer the line where it passes, the less it moves it.
    if (kick) {
      const gone = (fill - x) / top.medium;
      const s = kick.ago - gone;
      const [rise, fade, period] = kick.down ? [0.3, 0.85, 0.95] : [0.16, 0.55, 0.5];
      const calmer = 0.35 + 0.65 * Math.min(1, x / track);
      if (s > 0 && s < 3.2 * fade) y += calmer * kick.size * smooth(s / rise) * Math.exp(-s / fade - gone / 1.2) * Math.sin((TAU * s) / period);
    }
    into[k] = smooth(Math.min(x, fill - x) / 12) * limit(y);
  }
  return into;
}

/** The string at one moment: its height above its rest at x, the ribbon's turning there, and the string as it was a moment ago (`later[i]`, LATER[i] seconds behind), which the far particles follow. */
export type Strung = { at: (x: number) => number; twist: (x: number) => number; later: readonly ((x: number) => number)[] };

/** The track a line `fill` long reaches to at rest at `level`, where nothing else says. */
const trackOf = (fill: number, level: number) => fill / Math.max(level, 1e-6);

/**
 * The string along a line `fill` long at `t` (seconds), its knob at `level`, on a track `track`
 * long (by default as long as the line at rest at that notch), tapering out along it; moved by
 * `kick` if the notch has just changed. `pool`: arrays to work it out in, kept by a frame.
 */
export function effortString(fill: number, t: number, level: number, kick?: Kick, pool?: Float64Array[], track = trackOf(fill, level)): Strung {
  const e = energy(level);
  const n = Math.max(2, Math.ceil(fill / STEP) + 2);
  const m = Math.max(2, Math.ceil(fill / LATER_STEP) + 2);
  return {
    at: reader(heights(fill, level, t, kick, STEP, table(pool, 0, n), track), STEP),
    twist: (x) => 1.2 * wander(4441, (t - x / e.speed) / 3),
    later: LATER.map((lag, j) => reader(heights(fill, level, t - lag, kick && { ...kick, ago: kick.ago - lag }, LATER_STEP, table(pool, j + 1, m), track), LATER_STEP)),
  };
}

// ---- Particles ----------------------------------------------------------------------------------

/** A particle's own lot, drawn once from its seed and kept: where along the line it starts, how it drifts and stirs, how deep it is, its light, and how much of a crowd it takes for it to show (`keep`). */
type Lot = {
  start: number;
  keep: number;
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
    keep: hash(s0 + 46),
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
/** Each layer's lots, made as many as are wanted and kept. */
const LOTS: Lot[][] = [[], [], [], []];
function lots(layer: number, n: number) {
  const got = LOTS[layer] ?? [];
  for (let q = got.length; q < n; q++) got.push(lotOf(90001 + layer * 40961 + q * 131));
  return got;
}

/** The working ring's glint (stands-light's), and where along the line its colours start. */
const GLINT = glintColour("working");
const [GR, GG, GB] = [(GLINT >> 16) & 255, (GLINT >> 8) & 255, GLINT & 255];
const OFFSET = 37;
/** The ring's own seconds for its colours and its glints to go round (stands-light's, console.css "active"): effort's pace runs its clock faster or slower than theirs. */
const RING_CURRENT = 5.3;
const RING_SPARK = 3.7;

// ---- Clocks -------------------------------------------------------------------------------------

/** The motes' drift at the track's end over their drift at its start: the last notch's pace over the first's. */
const RHO = WILD.flow / CALM.flow;
const LN_RHO = Math.log(RHO);
/** A lap of the track at that profile takes this much longer than the track's length at the last notch's pace. */
const LAP = (RHO * LN_RHO) / (RHO - 1);

/**
 * The line's clocks: each counts on at a pace the knob's notch sets (the ribbon's turns, the
 * ring's seconds for its colours and for its glints), so a change of notch speeds each up or slows
 * it down from where it is, where a pace times the time would jump.
 */
export type Clocks = { turn: number; current: number; spark: number };
function paces(level: number): Clocks {
  const e = energy(level);
  return { turn: 1 / e.turn, current: RING_CURRENT / e.current, spark: RING_SPARK / e.spark };
}
/** The clocks at `t` as they'd stand had the notch always been `level`. */
export function clocksAt(t: number, level: number): Clocks {
  const p = paces(level);
  return { turn: t * p.turn, current: t * p.current, spark: t * p.spark };
}
/** Moves the clocks on `dt` seconds at the paces of `level`. */
export function tickClocks(c: Clocks, dt: number, level: number) {
  const p = paces(level);
  c.turn += dt * p.turn;
  c.current += dt * p.current;
  c.spark += dt * p.spark;
}

/** A particle's light: its colour from the ring where it is at the colours' clock (`clock`), leaning its own way and in its own tone; how bright (it twinkles; a glint or a flash lifts it) at the line's energy where it is (`e`); how big. Dust (`layer` 3) is finest and faintest. */
const lit = { c: 0, r: 0, a: 0, flash: 0, star: false };
function light(l: Lot, x: number, t: number, layer: number, b: number, e: Energy, clock: number, sh: Shine, fade: number) {
  const s = x + OFFSET;
  const g = glintAt(sh, s) * e.glint;
  const flash = b === 0 && l.sparkle < e.sparkle ? Math.max(0, Math.sin(TAU * (t * l.flashF + l.flashP))) ** 28 : 0;
  const twinkle = 1 - l.twinkle * (0.5 + 0.5 * Math.sin(TAU * (t * l.twinkleF + l.twinkleP)));
  let c = colourAt("working", s, clock, l.grain, 0.6 * g);
  if (l.tone < 0.14) c = mix(c, GR, GG, GB, 0.35);
  else if (l.tone > 0.84) c = dim(c, 0.76);
  if (flash > 0) c = mix(c, GR, GG, GB, flash);
  const star = layer === 0 && l.star;
  lit.c = c;
  lit.r = layer === 3 ? 0.12 + 0.18 * l.size : (0.16 + 0.55 * l.size ** 2.2 + (l.big ? 0.45 : 0) + (star ? 0.25 : 0)) * (1 + 0.4 * g + 0.9 * flash);
  const base = layer === 3 ? 0.15 + 0.3 * l.bright : 0.32 + 0.68 * l.bright ** 0.8;
  lit.a = Math.min(1, e.bright * base * twinkle * (star ? 1.4 : 1) + 0.3 * g + 0.6 * flash) * fade;
  lit.flash = flash;
  lit.star = star;
  return lit;
}
/** One particle: in focus (`b` 0) a crisp point; out of focus a soft disc, larger and fainter the further from focus. */
function particle(f: Frame, x: number, y: number, r: number, a: number, c: number, b: number) {
  if (a <= 0.01 || r <= 0) return;
  if (b === 0) f.dots.push(x, y, r, a, c);
  else f.soft.push(x, y, r * (3.5 + 12 * b), a * (0.85 - 0.35 * b), c, 1);
}

/** Each layer's drift, a share of energy's `flow`: the core slowest, the out-of-focus discs fastest. */
const DRIFT = [0.12, 0.25, 0.4, 0.18] as const;
/** A particle shows where the line has room for it (its `keep` under the share wanted there), coming in over EDGE of that share; SHOWN of the share shows on average, so the share asked for is over it. */
const EDGE = 0.3;
const SHOWN = 1 - EDGE / 2;

/**
 * How a particle stirs (−1 to 1) where the line is `m` up the notches: the first notch's slow beat
 * and the last's quick one, mixed by `m`, so it quickens along the line (a beat that changed with
 * the place would put neighbours further out of step every second). `jiggle`: its drift's own
 * small back and forth, at half its second beat.
 */
function sway(l: Lot, t: number, m: number) {
  const calm = 0.62 * Math.sin(TAU * ((CALM.freq[0] ?? 0) * l.f1 * t + l.p1)) + 0.38 * Math.sin(TAU * ((CALM.freq[1] ?? 0) * l.f2 * t + l.p2));
  const wild = 0.62 * Math.sin(TAU * ((WILD.freq[0] ?? 0) * l.f1 * t + l.p1)) + 0.38 * Math.sin(TAU * ((WILD.freq[1] ?? 0) * l.f2 * t + l.p2));
  return calm + (wild - calm) * m;
}
function jiggle(l: Lot, t: number, m: number) {
  const calm = Math.sin(TAU * ((CALM.freq[1] ?? 0) * l.f2 * 0.5 * t + l.p2));
  const wild = Math.sin(TAU * ((WILD.freq[1] ?? 0) * l.f2 * 0.5 * t + l.p2));
  return calm + (wild - calm) * m;
}

// ---- The stops ----------------------------------------------------------------------------------

/** A stop the line hasn't reached: the scale's own grey dot (run-settings.css .rs-tick), its radius in points. */
const STOP = 0x3a3c44;
const STOP_R = 2.5;
/** How much the line has reached the stop at x: over the last few points before it, the grey turns to light. */
const reached = (fill: number, x: number) => (fill < 1 ? 0 : smooth((fill + 3 - x) / 6));

/** How much less each stop past the knob stirs than the one before it, nearer the knob. */
const FALLOFF = 0.6;

/**
 * Each stop at `stops` (points along the track) the line hasn't reached: the grey dot it is,
 * stirring as the knob's light spills on to it, the one nearest the knob the most and each further
 * one FALLOFF as much (a little more the higher the notch): breathing a glow and the ring's colour,
 * the dot swelling and lifting a little with it, and at its calmest grey again.
 */
function greyStops(f: Frame, stops: readonly number[], gap: number, fill: number, level: number, t: number, clock: number) {
  for (const [i, x] of stops.entries()) {
    const on = reached(fill, x);
    if (on >= 1) continue;
    const near = FALLOFF ** Math.max(0, (x - fill) / gap - 1) * (0.85 + 0.15 * level);
    const p = hash(6211 + 89 * i);
    const calm = Math.sin(TAU * (0.18 * t + p));
    const quick = Math.sin(TAU * (0.35 * t + 1.3 * p));
    const stir = calm + (quick - calm) * near;
    const breath = 0.5 + 0.5 * stir;
    const c = colourAt("working", x + OFFSET, clock, 0, 0);
    const y = Y + 0.6 * near * stir;
    f.dots.push(x, y, STOP_R * (1 + near * (0.04 + 0.16 * breath)), 1 - on, mix(STOP, (c >> 16) & 255, (c >> 8) & 255, c & 255, near * (0.15 + 0.4 * breath)));
    f.soft.push(x, y, 8 + 6 * near * breath, near * (0.06 + 0.22 * breath) * (1 - on), c, 1);
  }
}

/**
 * Each stop the line has reached, as a bead of its light on the string: in the ring's colours,
 * the glints passing through it, its glow breathing, and as the line is where it sits, the further
 * along the larger, brighter and quicker (the first notch's slow breath and the last's quick one,
 * mixed, as the particles' stir is).
 */
function litStops(f: Frame, stops: readonly number[], fill: number, track: number, t: number, clock: number, S: Strung, sh: Shine) {
  for (const [i, x] of stops.entries()) {
    const on = reached(fill, x);
    if (on <= 0) continue;
    const at = Math.min(x, fill);
    const m = Math.min(1, at / track);
    const s = x + OFFSET;
    const g = glintAt(sh, s) * energyAt(m).glint;
    const p = hash(8191 + 97 * i);
    const calm = 0.5 + 0.5 * Math.sin(TAU * (0.2 * t + p));
    const wild = 0.5 + 0.5 * Math.sin(TAU * (1.1 * t + 1.7 * p));
    const breath = calm + (wild - calm) * m;
    // Its core a little larger and brighter than any particle, so it still marks its notch.
    const c = mix(colourAt("working", s, clock, 0, 0.6 * g), GR, GG, GB, 0.4);
    const y = Y + S.at(at);
    f.dots.push(x, y, (1.5 + 0.4 * m) * (1 + 0.1 * breath + 0.25 * g), (0.85 + 0.15 * m) * on, c);
    f.soft.push(x, y, (5 + 9 * m) * (0.85 + 0.3 * breath) * (1 + 0.5 * g), ((0.22 + 0.3 * m) * (0.6 + 0.4 * breath) + 0.3 * g) * on, c, 1);
  }
}

/** What else a frame is drawn with: a change of notch's `kick`, the scale's `stops` (points along the track, the last at its end), the line's `clocks` (as they'd stand at `t` at `level` if not given), and the frame to draw `into`. */
export type Scene = { kick?: Kick; stops?: readonly number[]; clocks?: Clocks; into?: Frame };

/**
 * The lit line at `t` (seconds): `fill` points of it from the scale's start to the knob at `level`
 * (0 the first notch, 1 the last), as stands-light's `paint` draws it. The line is the track's own,
 * each point as lively as the notch there, lit from the start to the knob: a change of notch moves
 * where it ends, and what's lit already stays as it was.
 */
export function effortFrame(fill: number, level: number, t: number, scene: Scene = {}): Frame {
  const { kick, stops = [], clocks = clocksAt(t, level), into: f = newFrame() } = scene;
  f.dots.length = 0;
  f.soft.length = 0;
  const track = stops.length > 1 ? (stops[stops.length - 1] ?? 0) : trackOf(fill, level);
  if (stops.length > 1) {
    const gap = Math.max(1e-6, (track - (stops[0] ?? 0)) / (stops.length - 1));
    greyStops(f, stops, gap, fill, level, t, clocks.current);
  }
  // Before the first notch's length there's no line, but on a track its spill still leaves the knob.
  if (track < 1 || (fill < 1 && stops.length < 2)) return f;
  /** How far up the notches the track is at x: the first at its start, the last at its end. */
  const up = (x: number) => Math.min(1, x / track);
  const S = effortString(fill, t, level, kick, f.scratch, track);
  const sh = shineOf("working", clocks.spark);
  // The swarm opens from a point at the start and is at its widest where it meets the knob.
  const ends = Math.max(1, Math.min(10, fill / 2.4));
  const taper = (x: number) => smooth(x / ends);
  // Past the knob the line spills on: as full as at the knob times `spills`, thinning away from it
  // (a third as full `reach` past it) and widening as it goes, with the knob's look. The higher the
  // notch, the fuller the line at the knob, the more of it spills and the further.
  const spills = 0.08 + 0.85 * level ** 1.2;
  const reach = track * (0.075 + 0.15 * level);
  const atKnob = Math.min(1, fill / track);
  for (let layer = 0; layer < 4; layer++) {
    // They drift along the whole track, faster the further along, RHO times as fast at its end as
    // at its start (each at its own multiple, `drift`), so they crowd where they're slow: of those
    // passing a point, as many show as the line holds there, or spills past the knob.
    const share = DRIFT[layer] ?? 0;
    const most = WILD.layers[layer] ?? 0;
    const n = Math.ceil(track * most * LAP);
    const kept = lots(layer, n);
    const follows = layer === 0 ? null : (S.later[Math.min(2, layer)] ?? S.at);
    for (let q = 0; q < n; q++) {
      const l = kept[q];
      if (!l) continue;
      // Where it's drifted to on its lap of the track, begun `start` of a lap ago: at a pace
      // growing with x, x grows exponentially through the lap.
      const lap = l.start + (l.drift * share * WILD.flow * t) / (LAP * track);
      const along = (track * Math.expm1((lap - Math.floor(lap)) * LN_RHO)) / (RHO - 1);
      const past = along - fill;
      if (past > 4.5 * reach) continue;
      const spilt = past > 0 ? spills * Math.exp(-past / reach) * smooth((track - along) / 6) : 1;
      const m = up(along);
      // Spilt, it keeps the knob's look.
      const look = past > 0 ? Math.min(m, atKnob) : m;
      const e = energyAt(look);
      const want = (((e.layers[layer] ?? 0) / most) * ((1 + (RHO - 1) * m) / RHO) * spilt) / SHOWN;
      const shown = clamp((want - l.keep) / (EDGE * want), 0, 1);
      if (shown <= 0) continue;
      const x = layer ? along : clamp(along + e.amp * 0.4 * l.live * jiggle(l, t, look), 0, track);
      const tp = taper(x);
      const fade = past > 0 ? (0.6 + 0.4 * Math.exp(-past / reach)) * smooth((track - x) / 4) : smooth(Math.min(x, fill - x) / (layer >= 2 ? 4 : 1.6));
      const open = past > 0 ? Math.min(2.5, 1 + (0.8 * past) / reach) : 1;
      const stir = l.live * e.amp * sway(l, t, look) * open;
      let y: number;
      let size = 1;
      let glow = 1;
      let b = 0;
      if (layer === 0) {
        // On the ribbon: across it, as it turns about the string, its near side larger and brighter.
        const sy = (l.later < 0.6 ? S.at : (S.later[0] ?? S.at))(x);
        const th = x * 0.05 - TAU * clocks.turn - S.twist(x);
        const depth = l.across * Math.sin(th);
        y = Y + sy + (l.across * e.half * open * Math.cos(th) + stir * 0.75) * tp;
        size = 1 + 0.35 * Math.max(0, depth);
        glow = 0.72 + 0.28 * depth;
      } else {
        // Floating about the string, following it a moment behind: most of the soft layer near
        // it, the out-of-focus discs anywhere within its spread, the dust further out.
        const off = (layer === 1 ? l.lane * e.spread * 0.5 : l.spread * e.spread * (layer === 2 ? 1 : 1.5)) * open;
        y = Y + (follows ?? S.at)(x) * (layer === 3 ? 0.6 : 0.85) + (off + stir * (1 + Math.min(layer, 2))) * tp;
        b = layer === 1 ? 0.25 + 0.4 * l.blur : layer === 2 ? 0.65 + 0.35 * l.blur : 0;
      }
      y = Y + inBox(y - Y);
      const lt = light(l, x, t, layer, b, e, clocks.current, sh, fade * shown);
      particle(f, x, y, lt.r * size * (layer === 2 ? 1.4 : 1), lt.a * glow, lt.c, b);
      if (lt.flash > 0.05) f.soft.push(x, y, 7 * lt.r, 0.5 * lt.flash * shown, lt.c, 1);
      if (lt.star) f.soft.push(x, y, 5 * lt.r, 0.35 * lt.a, lt.c, 1);
    }
  }
  // Each glint's sheen: a soft highlight along the string where it passes, as bright as the line is there.
  for (const at of sh.at) {
    for (let k = -1; ; k++) {
      const x = (k + at) * 110 - OFFSET;
      if (x > fill) break;
      if (x >= 0) f.soft.push(x, Y + S.at(x), 4.6, 0.55 * sh.o * energyAt(up(x)).glint * taper(x), GLINT, 2.8);
    }
  }
  litStops(f, stops, fill, track, t, clocks.current, S, sh);
  return f;
}
