/**
 * An opened commit's job cards (plan 01a175b7f21b7455bcdcd71e94ecdce0, step 1): its six stages as
 * cards, as many to a view as fit (6, 4, 3, 2 or 1 by the width there is), the cards and their gaps
 * together exactly the lanes card's width (ship.css). Previous and next show the view before and
 * after, each off at its end. Only the view's cards are drawn, so nothing scrolls sideways (W21-AC01).
 */
import * as React from "react";
import { ChevronLeft, ChevronRight } from "lucide-react";
import { shipApi, type ShipCommit, type ShipDetail } from "@/api/ship";
import { whoSaid } from "@/api/review";
import { StatusGlyph } from "@/components/ui/status-glyph";
import { GLYPHS } from "@/components/ui/status-glyph-spec";
import { glyphOf, pickCard, shipGroups, strip } from "./model";
import { reviewGroup, type ShipLane, type ShipLaneGroup } from "./review/lanes";
import { formatDuration } from "@/lib/format";
import { useSuspenseResource } from "@/lib/resource";
import { SectionHead } from "./head";

/** The cards: the journey's six stages after Review (Review is Files' to show). */
export const cardsOf = (commit: ShipCommit) => strip(commit.pips).filter((p) => p.stage !== "review");

/** The stage picked first, by the cards' rule. */
export const firstStage = (commit: ShipCommit) => {
  const cards = cardsOf(commit);
  return cards[pickCard(cards)]?.stage ?? null;
};

/** States of a job under way, the one shown before any is picked. */
const UNDER_WAY_PART = new Set(["running", "replanting", "guard", "soaking"]);

/** How many cards a view holds: the `--n` ship.css gives the track for the width there is. */
const perView = (el: HTMLElement) => Math.max(1, Number.parseInt(getComputedStyle(el).getPropertyValue("--n"), 10) || 1);

/** A job's time beside it: who for a review's mark, how long for one that ran, else why. */
function valueOf(l: ShipLane): string {
  if (l.by) return whoSaid(l.by);
  if (l.start != null && l.end != null && l.end > l.start) return formatDuration(l.end - l.start);
  return l.note ?? "";
}

/** The job shown until you pick one, as f-ship picks it: the one under way, else the one that failed, else the last begun. */
export function pickPart(groups: readonly ShipLaneGroup[]): string | null {
  const all = groups.flatMap((g) => g.lanes.map((l) => ({ key: `${g.group}|${l.id}`, l })));
  const begun = all.filter((x) => x.l.start != null);
  return (
    begun.find((x) => UNDER_WAY_PART.has(x.l.state))?.key ??
    begun.find((x) => x.l.state === "failed" || x.l.state === "rolledback")?.key ??
    begun.slice(-1)[0]?.key ??
    all.find((x) => x.l.job)?.key ??
    null
  );
}

/** A picked job's stage and lane, by its key. */
export function partOf(groups: readonly ShipLaneGroup[], key: string | null): { group: ShipLaneGroup; lane: ShipLane } | null {
  for (const group of groups) for (const lane of group.lanes) if (`${group.group}|${lane.id}` === key) return { group, lane };
  return null;
}

/** An opened commit's stations: Review first (its marks), then each of Ship's stages. */
export const stationsOf = (detail: ShipDetail): ShipLaneGroup[] => {
  const review = reviewGroup(detail);
  if (!review) return shipGroups(detail);
  // Its head says how many threads it has, as f-ship's does; with none, how it stands.
  const threads = new Set((detail.review ?? []).filter((m) => m.kind === "comment" && m.thread).map((m) => m.thread)).size;
  const note = threads ? `${threads} thread${threads === 1 ? "" : "s"}` : GLYPHS[glyphOf(review.state)].word;
  return [{ ...review, note }, ...shipGroups(detail)];
};

/**
 * An opened commit's Jobs as f-ship draws them (plan 01a1856fe81375b1bd1d1d86f270a45b, step 8): a
 * card for each stage, Review first, its head the stage's glyph, name and word (how many of its jobs
 * are done, or how it stands), and every job it has, each with its glyph, name and time (or who, or
 * why). As many cards to a view as fit (6, 4, 3, 2 or 1 by the width there is), the cards and their
 * gaps together exactly the lanes card's width (ship.css); previous and next page through them. The
 * job picked is the one Output shows (`picked`, `onPick`; until one is, pickPart's).
 */
export function JobCards({ commit, picked, onPick }: { commit: ShipCommit; picked: string | null; onPick: (key: string) => void }) {
  const detail = useSuspenseResource(`ship/commit/${commit.repo}/${commit.sha}`, (s) => shipApi.commit(commit.repo, commit.sha, s), { pollMs: 15_000, on: ["run", "job"] });
  const stations = React.useMemo(() => stationsOf(detail.data), [detail.data]);
  const shown = picked ?? pickPart(stations);
  // The view: `n` cards from `start`, read as ship.css lays the track out (before the first paint,
  // and again as the width changes); the last view is the last `n` cards, so every view is full.
  const [n, setN] = React.useState(1);
  const [start, setStart] = React.useState(0);
  const attach = React.useCallback((el: HTMLOListElement | null) => {
    if (!el) return;
    setN(perView(el));
    const seen = new ResizeObserver(() => setN(perView(el)));
    seen.observe(el);
    return () => seen.disconnect();
  }, []);
  const last = Math.max(0, stations.length - n);
  const from = Math.min(start, last);
  const page = (dir: 1 | -1) => setStart(Math.max(0, Math.min(last, from + dir * n)));
  return (
    <div className="ship-jobs">
      <SectionHead label="Jobs">
        <button type="button" className="ship-jobs__page" aria-label="Previous stages" disabled={from === 0} onClick={() => page(-1)}>
          <ChevronLeft size={16} />
        </button>
        <button type="button" className="ship-jobs__page" aria-label="Next stages" disabled={from >= last} onClick={() => page(1)}>
          <ChevronRight size={16} />
        </button>
      </SectionHead>
      <ol className="ship-jobs__track" ref={attach} aria-label="Stages">
        {stations.slice(from, from + n).map((g) => (
          <li key={g.group} className="ship-stn" data-stage={g.group} data-state={glyphOf(g.state)}>
            <header className="ship-stn__head">
              <StatusGlyph state={glyphOf(g.state)} size={14} />
              <span className="ship-stn__name">{g.label}</span>
              <span className="ship-stn__n">{g.note}</span>
            </header>
            <ul className="ship-stn__parts" aria-label={`${g.label} jobs`}>
              {g.lanes.map((l) => {
                const key = `${g.group}|${l.id}`;
                return (
                  <li key={l.id}>
                    <button type="button" className="ship-part" aria-pressed={key === shown} onClick={() => onPick(key)}>
                      <StatusGlyph state={glyphOf(l.state)} size={12} />
                      <span className="ship-part__name">{l.name}</span>
                      <span className="ship-part__v">{valueOf(l)}</span>
                    </button>
                  </li>
                );
              })}
            </ul>
          </li>
        ))}
      </ol>
    </div>
  );
}
