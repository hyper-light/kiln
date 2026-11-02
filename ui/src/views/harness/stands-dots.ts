/**
 * Mission control's rule as its agents (stands.tsx): a dot for each, the dots of each way they stand
 * together in a band a few rows tall, the bands side by side in the order they read. Each band's
 * left side is an arrowhead and its right side the notch the next one's arrowhead fits into, one
 * empty column between them; when there are more agents than fit across, every band grows two rows
 * (3, 5, 7…), so the arrowheads keep their point.
 *
 * Each dot is a small cluster of grains in its state's own colour (stands-light.ts `toneAt`),
 * stirring as its state does: working's turning quickest, ready's breathing, needs you's brightening
 * and dimming, finished's barely moving; stopped's a short dash, offline's a still point. Over them
 * each band plays a game of life of its own on its grid (the band's edges wrapping), lighting the
 * dots that live and dimming the rest, a lit dot flashing as it's born: working's generations come
 * quickest, finished's slowest, paused's hold where they were, and stopped and offline play none.
 * Every so many generations the band takes another rule of the life-like family (Life, HighLife,
 * Morley, Day & Night…) from where it is, and a pattern that dies out or stops changing is sown
 * again, so its patterns never settle.
 *
 * Everything here is pure: `layout` places the dots, `between` eases one placing into another as
 * counts change, `dotsFrame` draws them at a moment, and stands-light's `paint` paints them.
 */
import { calling, hash, newFrame, smooth, toneAt, type Frame, type Shown } from "./stands-light";

const TAU = 2 * Math.PI;

/** Points between two dots' middles along a row (at least, and at most once spread to fill), between two rows, and kept clear at the rule's edges. */
export const PITCH = 9;
const MOST_PITCH = 13;
const LINE = 8.5;
const MARGIN = 5;
/** The fewest rows a band has, and the most. */
const LEAST_ROWS = 3;
const MOST_ROWS = 15;

/** A dot's place: whose it is (its state and its number among them), where it is, and its cell in its band's grid. */
export type Slot = { stand: Shown; i: number; x: number; y: number; row: number; col: number };
/** The dots' places across a width, how many rows the bands have, and the height they take. */
export type Layout = { slots: Slot[]; rows: number; height: number; pitch: number };

/** The rows of a band in the order a column fills: the outer ones first, then in toward the middle, so a band's last column part full leaves the notch the next band's arrowhead fits. */
const orderOf = (rows: number) => {
  const mid = (rows - 1) / 2;
  return Array.from({ length: rows }, (_, r) => r).sort((a, b) => Math.abs(b - mid) - Math.abs(a - mid) || a - b);
};

/** How many columns the bands take with `rows` rows: each band's own, an empty one between each two, and the arrowhead's depth. */
const columnsOf = (groups: readonly (readonly [Shown, number])[], rows: number) => {
  const shown = groups.filter(([, n]) => n > 0);
  return shown.reduce((s, [, n]) => s + Math.ceil(n / rows), 0) + Math.max(0, shown.length - 1) + (rows - 1) / 2;
};

/** The dots of `groups` (each state and how many, in the order they read) across `w` points. */
export function layout(groups: readonly (readonly [Shown, number])[], w: number): Layout {
  let rows = LEAST_ROWS;
  while (rows < MOST_ROWS && MARGIN * 2 + (columnsOf(groups, rows) - 1) * PITCH > w) rows += 2;
  const columns = columnsOf(groups, rows);
  const pitch = columns > 1 ? Math.min(MOST_PITCH, Math.max(PITCH, (w - 2 * MARGIN) / (columns - 1))) : PITCH;
  const mid = (rows - 1) / 2;
  const order = orderOf(rows);
  const slots: Slot[] = [];
  let at = 0;
  for (const [stand, n] of groups) {
    if (n <= 0) continue;
    for (let i = 0; i < n; i++) {
      const col = Math.floor(i / rows);
      const row = order[i % rows] ?? 0;
      slots.push({ stand, i, row, col, x: MARGIN + (at + col + Math.abs(row - mid)) * pitch, y: MARGIN + row * LINE });
    }
    at += Math.ceil(n / rows) + 1;
  }
  return { slots, rows, height: 2 * MARGIN + (rows - 1) * LINE, pitch };
}

/** A dot as it's drawn: its place, and how much of it is there (`on`: 0 coming or going, 1 there). */
export type Placed = Slot & { on: number };

/** The dots `k` of the way (0 to 1) from placing `a` to placing `b`: each dot in both moving between its places, one only in `a` going where it was, one only in `b` coming where it'll be. */
export function between(a: Layout, b: Layout, k: number): { dots: Placed[]; height: number } {
  const e = smooth(k);
  const was = new Map(a.slots.map((s) => [`${s.stand}:${s.i}`, s]));
  const dots: Placed[] = [];
  for (const s of b.slots) {
    const p = was.get(`${s.stand}:${s.i}`);
    was.delete(`${s.stand}:${s.i}`);
    dots.push(p ? { ...s, x: p.x + (s.x - p.x) * e, y: p.y + (s.y - p.y) * e, on: 1 } : { ...s, on: e });
  }
  for (const p of was.values()) dots.push({ ...p, on: 1 - e });
  return { dots, height: e >= 1 ? b.height : Math.max(a.height, b.height) };
}

// ---- Life ---------------------------------------------------------------------------------------

/** The life-like rules a band takes in turn: how many neighbours bring a dot to life, and how many keep it living. */
const RULES: readonly (readonly [born: readonly number[], lives: readonly number[]])[] = [
  [[3], [2, 3]], // Life
  [
    [3, 6],
    [2, 3],
  ], // HighLife
  [
    [3, 6, 8],
    [2, 4, 5],
  ], // Morley
  [
    [3, 6, 7, 8],
    [3, 4, 6, 7, 8],
  ], // Day & Night
  [
    [3, 4],
    [3, 4],
  ], // 34 Life
  [
    [3, 5, 7],
    [1, 3, 5, 8],
  ], // Amoeba's cousin
];
/** Generations a band keeps one rule, and the share of dots sown alive when it's sown. */
const EPOCH = 36;
const SOWN = 0.38;
/** Seconds a generation lasts in each state (none: it plays no game). Paused holds the generation it had. */
const PACE: Partial<Record<Shown, number>> = { working: 0.32, ready: 1.1, needs: 0.8, paused: 1, finished: 2.6 };

type Grid = { rows: number; cols: number; n: number; order: readonly number[] };
/** The cell of dot `i`, and the dot in a cell (or -1 where the band's last column has none). */
const cellOf = (g: Grid, i: number) => (g.order[i % g.rows] ?? 0) * g.cols + Math.floor(i / g.rows);
function sow(g: Grid, seed: number, share: number) {
  const cells = new Uint8Array(g.rows * g.cols);
  for (let i = 0; i < g.n; i++) cells[cellOf(g, i)] = hash(seed + i * 31) < share ? 1 : 0;
  return cells;
}
/** One generation under `rule`, the grid's edges wrapping; cells with no dot stay empty. A band that's died out or stopped changing is sown again. */
function step(g: Grid, cells: Uint8Array, rule: number, seed: number, before?: Uint8Array) {
  const [born, lives] = RULES[rule % RULES.length] ?? RULES[0]!;
  const next = new Uint8Array(cells.length);
  const real = new Uint8Array(cells.length);
  for (let i = 0; i < g.n; i++) real[cellOf(g, i)] = 1;
  let alive = 0;
  let same = true;
  for (let r = 0; r < g.rows; r++)
    for (let c = 0; c < g.cols; c++) {
      const k = r * g.cols + c;
      if (!real[k]) continue;
      let around = 0;
      for (let dr = -1; dr <= 1; dr++)
        for (let dc = -1; dc <= 1; dc++) {
          if (!dr && !dc) continue;
          around += cells[((r + dr + g.rows) % g.rows) * g.cols + ((c + dc + g.cols) % g.cols)] ?? 0;
        }
      const v = (cells[k] ? lives : born).includes(around) ? 1 : 0;
      next[k] = v;
      alive += v;
      if (v !== (before ? before[k] : cells[k])) same = false;
    }
  if (alive === 0 || alive === g.n || same) {
    const sown = sow(g, seed, SOWN * 0.6);
    for (let k = 0; k < next.length; k++) next[k] = (next[k] ?? 0) ^ (sown[k] ?? 0);
  }
  return next;
}
/** The band's cells at generation `gen`: its epoch begun from where the epoch before it ended (that one from a sowing), a few dots flipped, under the epoch's rule. Kept, as the next frame asks again. */
const KEPT = new Map<string, Uint8Array>();
function lifeAt(stand: Shown, g: Grid, gen: number) {
  const epoch = Math.floor(gen / EPOCH);
  const key = `${stand}:${g.n}:${g.rows}:${gen}`;
  const hit = KEPT.get(key);
  if (hit) return hit;
  const seedOf = (e: number) => (stand.length * 7919 + e * 104729 + g.n * 13) | 0;
  const ruleOf = (e: number) => Math.floor(hash(seedOf(e) + 5) * RULES.length);
  const startKey = `${stand}:${g.n}:${g.rows}:start:${epoch}`;
  let cells = KEPT.get(startKey);
  if (!cells) {
    let c = sow(g, seedOf(epoch - 1), SOWN);
    let b: Uint8Array | undefined;
    for (let s = 0; s < EPOCH; s++) {
      const n = step(g, c, ruleOf(epoch - 1), seedOf(epoch - 1) + 97 * s, b);
      b = c;
      c = n;
    }
    const flips = sow(g, seedOf(epoch) + 11, 0.08);
    for (let k = 0; k < c.length; k++) c[k] = (c[k] ?? 0) ^ (flips[k] ?? 0);
    KEPT.set(startKey, (cells = c));
  }
  let b: Uint8Array | undefined;
  for (let s = epoch * EPOCH; s < gen; s++) {
    const n = step(g, cells, ruleOf(epoch), seedOf(epoch) + 97 * s, b);
    b = cells;
    cells = n;
  }
  if (KEPT.size > 400) KEPT.clear();
  KEPT.set(key, cells);
  return cells;
}

// ---- The dots -----------------------------------------------------------------------------------

/** How each state's dot looks: grains in it, how wide it is, how fast its grains turn about its middle and how far they stir, how bright it is lit and unlit, and stopped's dash. */
type Look = { grains: number; size: number; spin: number; stir: number; lit: number; dim: number; breathes?: boolean; calls?: boolean; dash?: number; point?: boolean };
const LOOKS: Record<Shown, Look> = {
  working: { grains: 18, size: 2.9, spin: 0.5, stir: 0.3, lit: 1, dim: 0.5 },
  ready: { grains: 15, size: 2.6, spin: 0.2, stir: 0.22, lit: 0.95, dim: 0.5, breathes: true },
  needs: { grains: 15, size: 2.6, spin: 0.15, stir: 0.2, lit: 1, dim: 0.55, calls: true },
  paused: { grains: 13, size: 2.4, spin: 0.05, stir: 0.1, lit: 0.9, dim: 0.5 },
  finished: { grains: 13, size: 2.3, spin: 0.03, stir: 0.06, lit: 0.88, dim: 0.48 },
  crashed: { grains: 8, size: 0, spin: 0, stir: 0.03, lit: 0.85, dim: 0.85, dash: 5 },
  offline: { grains: 1, size: 0, spin: 0, stir: 0, lit: 0.6, dim: 0.6, point: true },
};
const SEED: Record<Shown, number> = { working: 11, ready: 23, needs: 37, paused: 41, finished: 53, crashed: 67, offline: 71 };
/** The moment a paused band's game stopped at: it holds this generation. */
const HELD = 7;

/**
 * How lit a dot is at `p` of the way through its generation, from `was` to `now`: one coming to
 * life flashes up past its light and settles to it; one dying fades out over half a generation.
 */
const litAt = (was: number, now: number, p: number) => (now ? (was ? 1 : smooth(p / 0.12) * (1 + 0.45 * Math.exp(-5 * p))) : was ? 1 - smooth(p / 0.5) : 0);

/** The dots at `t` (seconds) into `f`, as stands-light's `paint` draws them. */
export function dotsFrame(dots: readonly Placed[], rows: number, t: number, f: Frame = newFrame()): Frame {
  f.dots.length = 0;
  f.soft.length = 0;
  // Each band's game: its grid and, at this moment, how lit each of its dots is.
  const lit = new Map<Shown, Float64Array>();
  const counts = new Map<Shown, number>();
  for (const d of dots) if (d.on >= 0.5) counts.set(d.stand, (counts.get(d.stand) ?? 0) + 1);
  for (const [stand, n] of counts) {
    const pace = PACE[stand];
    const out = new Float64Array(n);
    if (!pace) {
      out.fill(1);
    } else {
      const g: Grid = { rows, cols: Math.ceil(n / rows), n, order: orderOf(rows) };
      const clock = stand === "paused" ? HELD : t / pace + hash(SEED[stand]) * EPOCH;
      const gen = Math.floor(clock);
      const now = lifeAt(stand, g, gen);
      const was = lifeAt(stand, g, gen - 1);
      for (let i = 0; i < n; i++) out[i] = stand === "paused" ? (now[cellOf(g, i)] ?? 0) : litAt(was[cellOf(g, i)] ?? 0, now[cellOf(g, i)] ?? 0, clock - gen);
    }
    lit.set(stand, out);
  }
  for (const d of dots) {
    const L = LOOKS[d.stand];
    const s0 = SEED[d.stand] * 7919 + d.i * 131;
    const l = lit.get(d.stand)?.[d.i] ?? 0;
    const light = (L.dim + (L.lit - L.dim) * Math.min(1, l)) * (L.calls ? calling(t) : 1) * d.on;
    const flash = Math.max(0, l - 1);
    const s = d.x;
    if (L.point) {
      f.dots.push(d.x, d.y, 0.85 * d.on, light, toneAt(d.stand, s, t, 0, 0));
      continue;
    }
    if (L.dash) {
      for (let q = 0; q < L.grains; q++) {
        const x = d.x + (q / (L.grains - 1) - 0.5) * L.dash;
        f.dots.push(x, d.y + (hash(s0 + q) - 0.5) * 0.5, (0.38 + 0.2 * hash(s0 + q + 9)) * d.on, light * (0.75 + 0.25 * hash(s0 + q + 3)), toneAt(d.stand, x, t, 0, 2 * hash(s0 + q + 7) - 1));
      }
      continue;
    }
    const size = L.size * (L.breathes ? 0.88 + 0.12 * Math.sin(TAU * (t / 2.4 + hash(s0 + 2))) : 1) * (0.85 + 0.15 * Math.min(1, l)) * (0.4 + 0.6 * d.on);
    // Its middle a little larger and brighter, so a dot reads as one.
    f.dots.push(d.x, d.y, (0.95 + 0.2 * l) * d.on, Math.min(1, light * 1.1 + 0.3 * flash), toneAt(d.stand, s, t, 0, 0.3, 0.5 * l + flash));
    for (let q = 1; q < L.grains; q++) {
      const g = s0 + q * 17;
      const rr = size * hash(g) ** 1.25;
      const turn = TAU * (hash(g + 1) + L.spin * (0.6 + 0.8 * hash(g + 2)) * t * (hash(g + 3) < 0.5 ? -1 : 1));
      const stir = L.stir * Math.sin(TAU * (0.5 * t * (0.6 + hash(g + 4)) + hash(g + 5)));
      const x = d.x + (rr + stir) * Math.cos(turn);
      const y = d.y + (rr + stir) * Math.sin(turn) * 0.9;
      const a = Math.min(1, light * (0.65 + 0.35 * hash(g + 6)) + 0.25 * flash);
      f.dots.push(x, y, (0.26 + 0.3 * hash(g + 7) ** 2) * (0.6 + 0.4 * d.on), a, toneAt(d.stand, s + rr, t, (hash(g + 8) - 0.5) * 0.2, 2 * hash(g + 9) - 1, 0.35 * l + flash));
    }
  }
  return f;
}
