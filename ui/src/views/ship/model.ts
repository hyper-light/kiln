/**
 * Ship's list, the pure part (docs/ci.md W26-R01, R02): a change's folded commits under its tip,
 * the needs-you-first order by stand, the words of each stand and action, and the facets.
 */
import type { ShipCommit, ShipPip } from "@/api/ship";
import { GLYPHS, STAGES, type GlyphState, type Pip } from "@/components/ui/status-glyph-spec";

/** Each stand's place in the needs-you-first order, and its word. */
export const STANDS: { id: string; word: string }[] = [
  { id: "needs_you", word: "Needs you" },
  { id: "failing", word: "Failing" },
  { id: "rolled_back", word: "Rolled back" },
  { id: "landing", word: "Landing" },
  { id: "checking", word: "Checking" },
  { id: "building", word: "Building" },
  { id: "in_staging", word: "In staging" },
  { id: "shipping", word: "Shipping" },
  { id: "replanting", word: "Replanting" },
  { id: "waiting", word: "Waiting" },
  { id: "fixed", word: "Fixed" },
  { id: "shipped", word: "Shipped" },
  { id: "superseded", word: "Superseded" },
];

export function standWord(stand: string): string {
  return STANDS.find((s) => s.id === stand)?.word ?? stand.replace(/_/g, " ");
}

function rank(stand: string): number {
  const i = STANDS.findIndex((s) => s.id === stand);
  return i === -1 ? STANDS.length : i;
}

export const ACTION_WORD: Record<string, string> = {
  approve: "Approve",
  hold: "Hold",
  retry: "Retry",
  hand_off: "Hand off",
  review: "Review",
  retry_runner: "Retry runner",
};

/** A row: a commit, and the commits of its change folded under it. */
export interface Item {
  head: ShipCommit;
  riders: ShipCommit[];
}

/** The page's commits as rows: each folds under its change's tip when the page has the tip. */
export function fold(commits: readonly ShipCommit[]): Item[] {
  const tips = new Set(commits.filter((c) => !c.fold_under).map((c) => c.sha));
  const items: Item[] = [];
  const at = new Map<string, Item>();
  for (const c of commits) {
    if (c.fold_under && tips.has(c.fold_under)) continue;
    const item = { head: c, riders: [] };
    items.push(item);
    at.set(c.sha, item);
  }
  for (const c of commits) {
    if (c.fold_under) at.get(c.fold_under)?.riders.push(c);
  }
  return items;
}

export type Sort = "needs" | "newest";

/** Needs you first (by stand, then newest), or newest first as Ship numbers them. */
export function order(items: readonly Item[], sort: Sort): Item[] {
  if (sort === "newest") return [...items];
  return items
    .map((it, i) => ({ it, i }))
    .sort((a, b) => rank(a.it.head.stand) - rank(b.it.head.stand) || a.i - b.i)
    .map((x) => x.it);
}

/** The pips the journey strip draws: the stages and states it knows (others wait for their glyphs). */
export function strip(pips: readonly ShipPip[]): Pip[] {
  const stages = STAGES as readonly string[];
  return pips.filter((p) => stages.includes(p.stage) && p.state in GLYPHS).map((p) => ({ stage: p.stage as Pip["stage"], state: p.state as GlyphState, p: p.p }));
}

/** The review's word on the row, while its glyph is to come: `D41 · requested`. */
export function reviewWord(c: ShipCommit): string | null {
  const ch = c.change;
  if (!ch) return null;
  const state = c.pips.find((p) => p.stage === "review")?.state;
  return [ch.name, state && state !== "none" ? state : null].filter(Boolean).join(" · ");
}

export const HOSTS = [
  { id: "kiln", label: "kiln" },
  { id: "github", label: "GitHub" },
  { id: "gitlab", label: "GitLab" },
  { id: "bitbucket", label: "Bitbucket" },
];

export const KINDS = [
  { id: "diff", label: "Diff" },
  { id: "request", label: "Pull or change request" },
  { id: "commit", label: "Commit" },
];
