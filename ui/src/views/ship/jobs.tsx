/**
 * An opened commit's job cards (plan 01a175b7f21b7455bcdcd71e94ecdce0, step 1): its six stages as
 * cards, as many to a view as fit (6, 4, 3, 2 or 1 by the width there is), the cards and their gaps
 * together exactly the lanes card's width (ship.css). Previous and next show the view before and
 * after, each off at its end. Only the view's cards are drawn, so nothing scrolls sideways (W21-AC01).
 */
import * as React from "react";
import { ChevronLeft, ChevronRight } from "lucide-react";
import type { ShipCommit } from "@/api/ship";
import { StatusGlyph } from "@/components/ui/status-glyph";
import { GLYPHS, STAGE_WORD } from "@/components/ui/status-glyph-spec";
import { pickCard, strip } from "./model";

/** The cards: the journey's six stages after Review (Review is Files' to show). */
export const cardsOf = (commit: ShipCommit) => strip(commit.pips).filter((p) => p.stage !== "review");

/** The stage picked first, by the cards' rule. */
export const firstStage = (commit: ShipCommit) => {
  const cards = cardsOf(commit);
  return cards[pickCard(cards)]?.stage ?? null;
};

/** How many cards a view holds: the `--n` ship.css gives the track for the width there is. */
const perView = (el: HTMLElement) => Math.max(1, Number.parseInt(getComputedStyle(el).getPropertyValue("--n"), 10) || 1);

export function JobCards({ commit, onPick }: { commit: ShipCommit; onPick?: (stage: string) => void }) {
  const stages = cardsOf(commit);
  // The picked card's job is the one whose output shows: yours once you click one, else the rule's.
  const [picked, setPicked] = React.useState<string | null>(null);
  const shown = picked ?? stages[pickCard(stages)]?.stage;
  const pick = (stage: string) => {
    setPicked(stage);
    onPick?.(stage);
  };
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
  const last = Math.max(0, stages.length - n);
  const from = Math.min(start, last);
  const page = (dir: 1 | -1) => setStart(Math.max(0, Math.min(last, from + dir * n)));
  return (
    <div className="ship-jobs">
      <ol className="ship-jobs__track" ref={attach} aria-label="Stages">
        {stages.slice(from, from + n).map((p) => (
          <li key={p.stage} className="ship-job" data-stage={p.stage} data-state={p.state} data-picked={p.stage === shown || undefined}>
            <button type="button" className="ship-job__pick" aria-pressed={p.stage === shown} onClick={() => pick(p.stage)}>
              <span className="ship-job__head">
                <StatusGlyph state={p.state} size={16} p={p.p} />
                {STAGE_WORD[p.stage]}
              </span>
              <span className="ship-job__state">{GLYPHS[p.state].word}</span>
            </button>
          </li>
        ))}
      </ol>
      <div className="ship-jobs__pager">
        <button type="button" className="ship-jobs__page" aria-label="Previous stages" disabled={from === 0} onClick={() => page(-1)}>
          <ChevronLeft size={16} />
        </button>
        <button type="button" className="ship-jobs__page" aria-label="Next stages" disabled={from >= last} onClick={() => page(1)}>
          <ChevronRight size={16} />
        </button>
      </div>
    </div>
  );
}
