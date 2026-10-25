/**
 * A goal's steps as its goal stack draws them (goal-plan.tsx), worked out apart to be tested: in
 * the plan's own order, on the goal's line; steps next to each other that wait for the same steps
 * run at once and hang on one branch beside it, however many there are, so the plan is never more
 * than two lines wide; a crowded plan's finished stretch at its top folds into one line; and a
 * step that waits for something other than what's just above it says so in words, not with a long
 * line across the others.
 */
import type { Stand } from "@/views/plans/status";

/** A step as a goal's plan draws it. */
export interface GoalStep {
  /** Its number in its plan (or among its goal's parts), from 1. */
  n: number;
  title: string;
  /** The steps it waits for, by number, as it runs (`effectiveNeeds`). */
  needs: number[];
  stand: Stand;
  /** Who's at it, in a few words ("Devin · fedora"): shown while it's worked on or needs you. */
  who?: string;
  /** Its own parts (a plan step's acceptance criteria and tests): how many are met, of how many. */
  checks?: { met: number; of: number };
  /** Where it stands in its own word, when its stand's isn't the one ("Planning"). */
  word?: string;
}

export type GoalPlanRow =
  /** A step, on the goal's line or the branch beside it; `after`: what it waits for that no line shows; `start`: it waits for nothing, though it's further down. */
  | { kind: "step"; lane: "main" | "branch"; step: GoalStep; after: GoalStep[]; start: boolean }
  /** Where a branch leaves the goal's line: how many steps run at once on it. */
  | { kind: "fork"; count: number }
  /** Where a branch comes back to the goal's line. */
  | { kind: "join" }
  /** A finished stretch, folded into one line: how many steps it met. */
  | { kind: "folded"; count: number };

/** A plan of more steps than this is crowded: its finished stretch at the top folds into one line. */
export const CROWDED = 8;

const same = (a: readonly number[], b: readonly number[]) => a.length === b.length && a.every((n) => b.includes(n));

/** The rows a goal's steps draw as: folded (`open`: shown). */
export function goalPlanRows(steps: readonly GoalStep[], open = false): GoalPlanRow[] {
  // Steps next to each other that wait for the same steps (and not for each other) run at once.
  const groups: GoalStep[][] = [];
  for (const s of steps) {
    const last = groups.at(-1);
    const first = last?.[0];
    if (last && first && same(first.needs, s.needs) && !s.needs.some((d) => last.some((x) => x.n === d))) last.push(s);
    else groups.push([s]);
  }
  const rows: GoalPlanRow[] = [];
  // A crowded plan's finished stretch at its top: one line, until it's shown.
  let from = 0;
  if (!open && steps.length > CROWDED) {
    while (from < groups.length && (groups[from] ?? []).every((s) => s.stand === "met")) from += 1;
    const count = groups.slice(0, from).reduce((k, g) => k + g.length, 0);
    if (count >= 2) rows.push({ kind: "folded", count });
    else from = 0;
  }
  for (let i = from; i < groups.length; i += 1) {
    const group = groups[i] ?? [];
    const needs = group[0]?.needs ?? [];
    // What's just above it is what a line shows it waits for; anything else is said.
    const above = (groups[i - 1] ?? []).map((s) => s.n);
    const after = same(needs, above) ? [] : needs.map((d) => steps.find((x) => x.n === d)).filter((x): x is GoalStep => !!x);
    const start = i > 0 && needs.length === 0;
    if (group.length === 1 && group[0]) {
      rows.push({ kind: "step", lane: "main", step: group[0], after, start });
      continue;
    }
    rows.push({ kind: "fork", count: group.length });
    for (const s of group) rows.push({ kind: "step", lane: "branch", step: s, after, start });
    rows.push({ kind: "join" });
  }
  return rows;
}

/** A step's title as another step speaks of it: up to its colon or dash, at most `max` characters. */
export function shortTitle(title: string, max = 40): string {
  const head = title.split(/:\s| — | – /)[0]?.trim() || title.trim();
  return head.length > max ? `${head.slice(0, max - 1).replace(/\s+\S*$/, "")}…` : head;
}

/** Where an agent runs, as a step says who's at it: a runner's name to its first dot, "the server" for kiln's own. */
export function machineWord(target: string): string {
  const name = target.replace(/^agent:/, "");
  return name === "local" ? "the server" : (name.split(".")[0] ?? name);
}
