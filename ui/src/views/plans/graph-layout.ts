/** A plan's steps laid out as their graph (graph.tsx): in rounds, and what each waits for. */
import type { Stand } from "./status";

export interface GraphStep {
  /** Its number, from 1. */
  n: number;
  title: string;
  /** The steps it waits for, by number. */
  needs: number[];
  stand?: Stand;
  /** Who does it, in a few words; and where to open it. */
  who?: string;
  href?: string;
}

/** The steps by round: each after the latest of the steps it waits for (a loop, if one were there, last). */
export function rounds(steps: GraphStep[]): GraphStep[][] {
  const round = new Map<number, number>();
  const at = (s: GraphStep, seen: Set<number>): number => {
    const known = round.get(s.n);
    if (known !== undefined) return known;
    if (seen.has(s.n)) return 0;
    seen.add(s.n);
    const before = s.needs.map((d) => steps.find((x) => x.n === d)).filter((x): x is GraphStep => !!x);
    const r = before.length ? Math.max(...before.map((b) => at(b, seen))) + 1 : 0;
    round.set(s.n, r);
    return r;
  };
  const out: GraphStep[][] = [];
  for (const s of steps) (out[at(s, new Set())] ??= []).push(s);
  return out.filter(Boolean);
}

/** What each step waits for, as a plan's steps run: as they say, or, none saying, the one before. */
export function effectiveNeeds(needs: (number[] | undefined)[]): number[][] {
  const any = needs.some((d) => d && d.length);
  return needs.map((d, i) => (any ? (d ?? []) : i === 0 ? [] : [i]));
}

/** A step's box in the graph, from the graph's own top left. */
export interface Box {
  left: number;
  top: number;
  right: number;
  bottom: number;
}

/**
 * The points a line from one step to the next passes through, as it's drawn: a curve out of the
 * one's right side into the other's left when the second is across from it, else out of its
 * bottom into the other's top.
 */
export function curve(x1: number, y1: number, x2: number, y2: number, across: boolean, n = 24): [number, number][] {
  const [c1, c2]: [[number, number], [number, number]] = across
    ? [
        [x1 + 18, y1],
        [x2 - 18, y2],
      ]
    : [
        [x1, y1 + 12],
        [x2, y2 - 12],
      ];
  return Array.from({ length: n + 1 }, (_, i) => {
    const t = i / n;
    const u = 1 - t;
    return [u * u * u * x1 + 3 * u * u * t * c1[0] + 3 * u * t * t * c2[0] + t * t * t * x2, u * u * u * y1 + 3 * u * u * t * c1[1] + 3 * u * t * t * c2[1] + t * t * t * y2];
  });
}

/** Whether a line passes through any of `others` (steps it would run behind): their step says "after …" instead. */
export function blocked(points: readonly [number, number][], others: readonly Box[], margin = 3): boolean {
  return points.some(([x, y]) => others.some((b) => x > b.left + margin && x < b.right - margin && y > b.top + margin && y < b.bottom - margin));
}
