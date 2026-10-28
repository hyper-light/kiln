/**
 * Ship's list, the pure part (docs/ci.md W26-R01, R02): a change's folded commits under its tip,
 * the needs-you-first order by stand, the words of each stand and action, and the facets.
 */
import type { ShipCommit, ShipDetail, ShipPip } from "@/api/ship";
import { GLYPHS, STAGE_WORD, type GlyphState, type Pip, type Stage } from "@/components/ui/status-glyph-spec";
import type { ShipLane, ShipLaneGroup } from "./review/lanes";

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
  /** What folds under it: its change's other commits, the commits riding with its approval, or the shipped ones after it. */
  folds?: "change" | "rides" | "shipped";
}

/** A row's key, stable as it changes: a change by its name (a new revision patches it in place), a commit by its sha. */
export const keyOf = (it: Item) => (it.head.change ? `${it.head.repo}/${it.head.change.name}` : `${it.head.repo}/${it.head.sha}`);

/**
 * The page's commits as rows: each folds under its change's tip when the page has the tip. A row
 * open on the page (its key in `open`) never folds under another: it keeps its row and its latest
 * data (opened commit's step 3, plan 01a175b7f21b7455bcdcd71e94ecdce0).
 */
export function fold(commits: readonly ShipCommit[], open: ReadonlySet<string> = new Set()): Item[] {
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
    const it = c.fold_under ? at.get(c.fold_under) : undefined;
    if (it) {
      it.riders.push(c);
      it.folds = "change";
    }
  }
  // Riders fold under the approval they ride with (the commit production waits on in their repository),
  // and a run of shipped commits under its newest (plan 01a175b7f20b7277b15bddd2ad001bcc, step 5).
  const approval = new Map<string, Item>();
  for (const it of items) if (it.head.actions?.some((a) => a.name === "approve")) approval.set(it.head.repo, it);
  const plain = (it: Item) => !it.head.change && it.riders.length === 0 && !open.has(keyOf(it));
  const out: Item[] = [];
  let shipped: Item | null = null;
  for (const it of items) {
    const to = approval.get(it.head.repo);
    if (to && to !== it && it.head.stand === "waiting" && plain(it)) {
      to.riders.push(it.head);
      to.folds ??= "rides";
      continue;
    }
    if (it.head.stand === "shipped" && plain(it)) {
      if (shipped && shipped.head.repo === it.head.repo) {
        shipped.riders.push(it.head);
        shipped.folds = "shipped";
        continue;
      }
      shipped = it;
    } else shipped = null;
    out.push(it);
  }
  return out;
}

export type Sort = "needs" | "newest" | "trunk" | "stage" | "author" | "oldest";

/** Ship's sorts, offered as Mission control's list bar offers its own (plan 01a175b7f20b7277b15bddd2ad001bcc, step 4). */
export const SORTS: { value: Sort; label: string }[] = [
  { value: "needs", label: "Needs you first" },
  { value: "newest", label: "Newest" },
  { value: "trunk", label: "Trunk order" },
  { value: "stage", label: "By stage" },
  { value: "author", label: "By author" },
  { value: "oldest", label: "Oldest" },
];
export const sortOf = (s: string | null): Sort => SORTS.find((x) => x.value === s)?.value ?? "needs";

/** How far an item has come: its furthest stage begun (Review 0 to Replant 6), -1 for none. */
const reached = (it: Item) => it.head.pips.reduce((far, p, i) => (p.state !== "none" ? i : far), -1);

/** The items in `sort`'s order; ties keep Ship's own (trunk) order. */
export function order(items: readonly Item[], sort: Sort): Item[] {
  const by: Record<Sort, (a: Item, b: Item) => number> = {
    needs: (a, b) => rank(a.head.stand) - rank(b.head.stand),
    newest: (a, b) => b.head.at - a.head.at,
    trunk: (a, b) => b.head.cursor - a.head.cursor,
    stage: (a, b) => reached(b) - reached(a),
    author: (a, b) => a.head.author.localeCompare(b.head.author),
    oldest: (a, b) => a.head.at - b.head.at,
  };
  return items
    .map((it, i) => ({ it, i }))
    .sort((a, b) => by[sort](a.it, b.it) || a.i - b.i)
    .map((x) => x.it);
}

/** A repository's project: its owner, the part before the slash (the read model links it to no other). */
export const projectOf = (repo: string) => repo.split("/")[0] ?? repo;

/** A facet's options from what the list holds: each value once, sorted. */
export const optionsOf = (values: readonly string[], label: (v: string) => string = (v) => v) => [...new Set(values.filter(Boolean))].sort().map((id) => ({ id, label: label(id) }));

/** The facets Ship filters by, in the list bar's order. */
export const FACET_KEYS = ["repo", "project", "author", "stand", "host", "kind"] as const;

/** The pips the journey strip draws, Review first, every one the server sends; a state the page has no glyph for yet shows as not yet. */
export function strip(pips: readonly ShipPip[]): Pip[] {
  return pips.map((p) => ({ stage: p.stage as Pip["stage"], state: (p.state in GLYPHS ? p.state : "none") as GlyphState, p: p.p }));
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

/** The stands Ship counts, in Mission control's words and order (plan 01a175b7f20b7277b15bddd2ad001bcc, step 3). */
export const SHIP_STANDS = ["needs", "crashed", "paused", "working", "finished"] as const;
type ShipShown = (typeof SHIP_STANDS)[number];

/** Ship's stands on Mission control's: needs you; failing and rolled back as stopped; in staging and
 * waiting as paused; in flight and shipping as working; shipped, fixed and superseded as finished. */
const STAND_OF: Record<string, ShipShown> = {
  needs_you: "needs",
  failing: "crashed",
  rolled_back: "crashed",
  in_staging: "paused",
  waiting: "paused",
  landing: "working",
  checking: "working",
  building: "working",
  shipping: "working",
  replanting: "working",
  shipped: "finished",
  fixed: "finished",
  superseded: "finished",
};

/** How many of the list's items stand each way (a commit folded under its change's tip isn't one). */
export function standCounts(commits: readonly ShipCommit[]): Record<ShipShown, number> {
  const out: Record<ShipShown, number> = { needs: 0, crashed: 0, paused: 0, working: 0, finished: 0 };
  for (const c of commits) {
    const s = c.fold_under ? undefined : STAND_OF[c.stand];
    if (s) out[s] += 1;
  }
  return out;
}

/** States a card's job is under way in, and those it failed in (opened commit's Output, step 2). */
const UNDER_WAY = new Set(["running", "soaking", "guard", "replanting"]);
const FELL = new Set(["failed", "error", "rolledback", "partial"]);

/**
 * The card picked first, as kiln's stepToShow picks a step: the running one, else the one that
 * failed, else the last that ran (the first when none has).
 */
export function pickCard(cards: readonly { state: string }[]): number {
  const running = cards.findIndex((c) => UNDER_WAY.has(c.state));
  if (running >= 0) return running;
  const failed = cards.findIndex((c) => FELL.has(c.state));
  if (failed >= 0) return failed;
  for (let i = cards.length - 1; i >= 0; i--) {
    const st = cards[i]?.state;
    if (st && st !== "none" && st !== "queued") return i;
  }
  return 0;
}

/** A deployment's state as a glyph's (W26-R14's lanes). */
const DEPLOY_GLYPH: Record<string, string> = { success: "passed", failure: "failed", error: "failed", inactive: "superseded", in_progress: "running", pending: "queued", queued: "queued" };

/**
 * Ship's stages as the Lanes' groups (W26-R14), in the journey's order after Review: each its pip's
 * state, and a lane for what the detail times (each Checks leg that ran, each deploy).
 */
export function shipGroups(detail: ShipDetail): ShipLaneGroup[] {
  const pip = (stage: Stage) => detail.pips.find((p) => p.stage === stage)?.state ?? "none";
  const legs: ShipLane[] = (detail.checks ?? []).flatMap((c) =>
    c.legs.flatMap((l) =>
      // Keyed by the leg, not its job: a leg tried again keeps its lane (opened commit's step 3).
      l.start === undefined ? [] : [{ id: `${c.pipeline}/${l.name}`, name: l.name, state: l.state, start: l.start, end: l.end ?? null, ...(l.note ? { note: l.note } : {}) }],
    ),
  );
  const deploys = (production: boolean): ShipLane[] =>
    (detail.deployments ?? [])
      .filter((d) => d.production === production)
      .map((d) => ({ id: `${d.environment}@${d.at}`, name: d.environment, state: DEPLOY_GLYPH[d.state ?? ""] ?? "none", start: d.at, end: d.at, ...(d.description ? { note: d.description } : {}) }));
  const group = (stage: Stage, lanes: ShipLane[]): ShipLaneGroup => ({ group: stage, label: STAGE_WORD[stage], state: pip(stage), note: "", lanes });
  return [group("land", []), group("checks", legs), group("build", []), group("staging", deploys(false)), group("production", deploys(true)), group("replant", [])];
}
