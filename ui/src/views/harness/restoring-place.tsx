import * as React from "react";
import { ArrowDown } from "lucide-react";
import { useSession, type SessionStore } from "@/api/harness";
import { restoreCanonicalPlace } from "./restore-place";
import type { RowPosition } from "./row-window";
import { LoadingMark, PrismLabel } from "./signals";

/**
 * Callback-ref ownership cancels navigation/Latest without a hidden controller or an effect. While it
 * restores, the conversation's loader stands where the conversation goes, as at its first load
 * (early.tsx), with the way to the latest messages the page always has (timeline.tsx's `hx-waiting`);
 * a place it can't restore goes to them.
 */
export function RestoringPlace({ store, position, resolved, latest }: { store: SessionStore; position: RowPosition; resolved: (position: RowPosition) => boolean; latest: () => void }) {
  const ready = useSession(store, (snapshot) => snapshot.ready);
  const ref = React.useCallback(
    (el: HTMLDivElement | null) => {
      if (!el || !ready) return;
      const controller = new AbortController();
      const giveUp = () => {
        if (!controller.signal.aborted) latest();
      };
      void restoreCanonicalPlace(store, position, controller.signal, () => {})
        .then((result) => {
          if (controller.signal.aborted || result.type === "cancelled") return;
          if (result.type === "error" || !resolved(result.position)) giveUp();
        })
        .catch(giveUp);
      return () => controller.abort();
    },
    [store, position, resolved, latest, ready],
  );
  return (
    <>
      <div ref={ref} className="ag-loading hx-restore-place" role="status">
        <LoadingMark />
        <PrismLabel live>Loading conversation</PrismLabel>
      </div>
      <div className="hx-waiting-anchor">
        <button type="button" className="hx-waiting" onClick={latest}>
          Latest
          <ArrowDown size={12} aria-hidden="true" />
        </button>
      </div>
    </>
  );
}
