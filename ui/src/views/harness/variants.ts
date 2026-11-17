/**
 * Variants of an experiment (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, P7.1), as people tell them
 * apart: never by a letter (kiln's research grades are lettered), by their swatch, a colour along
 * kiln's prism, and by their name, by default what sets each apart from the others. And their
 * weights, which always add up to 100, and their measures on axes they share (P7.2).
 */
import type { Measure, Variant } from "@/api/agentdefs";

/** A variant's colour: its place along kiln's prism (index.css --prism), so any number of variants reads apart, in kiln's own colours. */
const PRISM: [number, number, number][] = [
  [0xb8, 0xd9, 0xd2],
  [0xa6, 0xc4, 0xed],
  [0xb8, 0xa6, 0xd5],
  [0xd6, 0xa2, 0xaa],
  [0xd8, 0xcb, 0xb0],
];
export function variantTone(i: number, n: number): string {
  const t = n <= 1 ? 0 : (i / (n - 1)) * (PRISM.length - 1);
  const k = Math.min(PRISM.length - 2, Math.floor(t));
  const f = t - k;
  const a = PRISM[k]!,
    b = PRISM[k + 1]!;
  return `rgb(${a.map((x, j) => Math.round(x + (b[j]! - x) * f)).join(" ")})`;
}

/** What a deployment can differ in, in the order a name says it: a host has no Agentfile or size. */
export type Deployment = Pick<Variant, "agentfile" | "engine" | "cpus" | "memory"> & { name?: string | null };
/** One thing a name can say of a deployment, `names` its Agentfiles' by their ids: none where it doesn't apply (a host's Agentfile and size). */
type Part = (v: Deployment, names?: ReadonlyMap<string, string>) => string | null;
const ENGINE: Part = (v) => (v.engine === "host" ? "host" : "microVM");
const SETS_APART: Part[] = [
  (v, names) => (v.agentfile ? (names?.get(v.agentfile) ?? v.agentfile) : null),
  ENGINE,
  (v) => (v.cpus === undefined ? null : `${v.cpus} vCPU`),
  (v) => (v.memory === undefined ? null : `${Math.round(v.memory / 2 ** 30)} GiB`),
];
/**
 * A variant's name: the one a person gave it, else what sets it apart from the others, in the order
 * above ("2 vCPU · 4 GiB" beside "4 vCPU · 8 GiB"), each Agentfile by its name in `names` (by its
 * `agentfile:<id>`), and a variant on the host "host". Two alike in all of that (differing in weight
 * alone) are told apart by their place among the alike: "2 vCPU · 4 GiB (2)".
 */
export function variantName(vs: readonly Deployment[], i: number, names?: ReadonlyMap<string, string>): string {
  const v = vs[i];
  if (!v) return "";
  if (v.name) return v.name;
  const said = (f: Part, o: Deployment) => f(o, names);
  // A part sets them apart where those it applies to differ in it (that one's on the host, the engine says).
  const differ = SETS_APART.filter((f) => new Set(vs.map((o) => said(f, o)).filter((x) => x !== null)).size > 1);
  const by = differ.length ? differ : v.engine === "host" ? [ENGINE] : SETS_APART.slice(2);
  const name = by
    .map((f) => said(f, v))
    .filter((x) => x !== null)
    .join(" · ");
  const alike = vs.filter((o, j) => j < i && !o.name && by.every((f) => said(f, o) === said(f, v))).length;
  return alike ? `${name} (${alike + 1})` : name;
}

/**
 * `parts` scaled in proportion to add up to `total`, each whole, as kiln scales an experiment's weights once a
 * removed variant's share goes back to the others (kiln_ci::split::rescale, kiln plan 01a1cd8e862e7169a074db3d5fa33f3d
 * P7.1): each its proportion rounded down, then one more to those with the largest remainders (the earliest first
 * among equals) until they add up. None when they add up to nothing.
 */
export function rescale(parts: readonly number[], total = 100): number[] | null {
  const sum = parts.reduce((a, x) => a + x, 0);
  if (sum === 0) return null;
  const out = parts.map((p) => Math.floor((p * total) / sum));
  const left = total - out.reduce((a, x) => a + x, 0);
  const order = parts.map((p, k) => [(p * total) % sum, k] as const).sort((a, b) => b[0] - a[0] || a[1] - b[1]);
  for (const [, k] of order.slice(0, left)) out[k] = (out[k] ?? 0) + 1;
  return out;
}

/** Moving one weight moves the others in proportion, so they always add up to 100. */
export function rebalance(ws: readonly number[], i: number, w: number): number[] {
  const rest = 100 - w;
  const others = ws.reduce((a, x, j) => (j === i ? a : a + x), 0);
  const out = ws.map((x, j) => (j === i ? w : others ? Math.round((x / others) * rest) : Math.round(rest / (ws.length - 1))));
  const drift = 100 - out.reduce((a, x) => a + x, 0);
  const last = out.length - 1 === i ? out.length - 2 : out.length - 1;
  out[last] = (out[last] ?? 0) + drift;
  return out;
}

/**
 * Measure `k`'s axis across an experiment's variants (kiln plan 01a1dd2ea7a777a28970a43fc5f766d0 P7.2): from the lowest
 * of their 95% intervals' lows to the highest of their highs, so every variant's interval is drawn to the same scale;
 * none where no variant has measures.
 */
export function measureAxis(vs: readonly Pick<Variant, "measures">[], k: Measure): [number, number] | null {
  const ivs = vs.flatMap((v) => (v.measures ? [v.measures[k]] : []));
  if (ivs.length === 0) return null;
  return [Math.min(...ivs.map((iv) => iv[0])), Math.max(...ivs.map((iv) => iv[2]))];
}

/** Where `x` falls along `axis`, as a percent of its length: its middle where the axis is one value. */
export function onAxis(x: number, [lo, hi]: readonly [number, number]): number {
  return hi > lo ? ((x - lo) / (hi - lo)) * 100 : 50;
}

/**
 * Whether variant `i` clearly wins measure `k`: its whole 95% interval below every other variant's, so none overlaps
 * it (each measure is better lower: a faster warm start, less memory, fewer failures); only where every variant has
 * measures.
 */
export function winsMeasure(vs: readonly Pick<Variant, "measures">[], i: number, k: Measure): boolean {
  const m = vs[i]?.measures;
  return m !== undefined && vs.every((o, j) => j === i || (o.measures !== undefined && m[k][2] < o.measures[k][0]));
}
