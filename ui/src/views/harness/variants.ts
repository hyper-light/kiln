/**
 * Variants of an experiment (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, P7.1), as people tell them
 * apart: never by a letter (kiln's research grades are lettered), by their swatch, a colour along
 * kiln's prism, and by their name, by default what sets each apart from the others. And their
 * weights, which always add up to 100.
 */
import type { Variant } from "@/api/agentdefs";

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

/** What a deployment can differ in, in the order a name says it. */
export type Deployment = Pick<Variant, "agentfile" | "engine" | "cpus" | "memory"> & { name?: string | null };
const SETS_APART: ((v: Deployment) => string)[] = [(v) => v.agentfile, (v) => (v.engine === "host" ? "host" : "microVM"), (v) => `${v.cpus} vCPU`, (v) => `${Math.round(v.memory / 2 ** 30)} GiB`];
/**
 * A variant's name: the one a person gave it, else what sets it apart from the others, in the order
 * above ("2 vCPU · 4 GiB" beside "4 vCPU · 8 GiB"). Two alike in all of that (differing in weight
 * alone) are told apart by their place among the alike: "2 vCPU · 4 GiB (2)".
 */
export function variantName(vs: readonly Deployment[], i: number): string {
  const v = vs[i];
  if (!v) return "";
  if (v.name) return v.name;
  const differ = SETS_APART.filter((f) => new Set(vs.map(f)).size > 1);
  const by = differ.length ? differ : SETS_APART.slice(2);
  const name = by.map((f) => f(v)).join(" · ");
  const alike = vs.filter((o, j) => j < i && !o.name && by.every((f) => f(o) === f(v))).length;
  return alike ? `${name} (${alike + 1})` : name;
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
