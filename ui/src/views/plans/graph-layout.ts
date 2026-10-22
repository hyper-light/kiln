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
