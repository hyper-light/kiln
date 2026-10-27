/**
 * An opened commit's job cards (plan 01a175b7f21b7455bcdcd71e94ecdce0, step 1): its six stages as
 * cards, as many to a view as fit (6, 4, 3, 2 or 1 by the width there is), the cards and their gaps
 * together exactly the lanes card's width (ship.css); they snap a card at a time, and previous and
 * next page by a view, each off at its end.
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

/** Whether the track is at its start and at its end. */
const endsOf = (el: HTMLElement) => ({ start: el.scrollLeft <= 1, end: el.scrollLeft + el.clientWidth >= el.scrollWidth - 1 });

export function JobCards({ commit, onPick }: { commit: ShipCommit; onPick?: (stage: string) => void }) {
  const stages = cardsOf(commit);
  // The picked card's job is the one whose output shows: yours once you click one, else the rule's.
  const [picked, setPicked] = React.useState<string | null>(null);
  const shown = picked ?? stages[pickCard(stages)]?.stage;
  const pick = (stage: string) => {
    setPicked(stage);
    onPick?.(stage);
  };
  const [ends, setEnds] = React.useState({ start: true, end: true });
  const track = React.useRef<HTMLOListElement | null>(null);
  const attach = React.useCallback((el: HTMLOListElement | null) => {
    track.current = el;
    if (!el) return;
    setEnds(endsOf(el));
    const seen = new ResizeObserver(() => setEnds(endsOf(el)));
    seen.observe(el);
    return () => seen.disconnect();
  }, []);
  // A page is a view: the track's width and one gap, so the next view starts on a card's edge.
  const page = (dir: 1 | -1) => {
    const el = track.current;
    if (!el) return;
    const gap = parseFloat(getComputedStyle(el).columnGap) || 0;
    el.scrollBy({ left: dir * (el.clientWidth + gap), behavior: document.documentElement.dataset.motion === "paused" ? "auto" : "smooth" });
  };
  return (
    <div className="ship-jobs">
      <ol className="ship-jobs__track" ref={attach} onScroll={(e) => setEnds(endsOf(e.currentTarget))} aria-label="Stages">
        {stages.map((p) => (
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
        <button type="button" className="ship-jobs__page" aria-label="Previous stages" disabled={ends.start} onClick={() => page(-1)}>
          <ChevronLeft size={16} />
        </button>
        <button type="button" className="ship-jobs__page" aria-label="Next stages" disabled={ends.end} onClick={() => page(1)}>
          <ChevronRight size={16} />
        </button>
      </div>
    </div>
  );
}
