import * as React from "react";
import { useSession, type SessionStore } from "@/api/harness";
import { survivingPosition, type RowPosition } from "./row-window";

type RestoreStore = Pick<SessionStore, "get" | "event" | "loadOlder" | "cancelOlder">;
export type RestoreResult = { type: "found"; position: RowPosition } | { type: "cancelled" } | { type: "error"; message: string };

/** Let the store publish its RAF-batched canonical page before inspecting the next one. */
const publishFrame = (signal: AbortSignal) =>
  new Promise<void>((resolve) => {
    let frame = 0;
    const finish = () => {
      cancelAnimationFrame(frame);
      signal.removeEventListener("abort", finish);
      resolve();
    };
    signal.addEventListener("abort", finish, { once: true });
    if (signal.aborted) finish();
    else frame = requestAnimationFrame(finish);
  });

/** W21-R02: restoring an actual saved reading position demands canonical history, one page at a time. */
export async function restoreCanonicalPlace(store: RestoreStore, position: RowPosition, signal: AbortSignal, progress: (pages: number) => void, yieldFrame = publishFrame): Promise<RestoreResult> {
  const cursor = store.get().history?.cursor;
  let pages = 0;
  while (!signal.aborted) {
    const snapshot = store.get();
    const history = snapshot.history;
    if (!snapshot.ready || (cursor !== undefined && history?.cursor !== cursor)) return { type: "error", message: "The conversation changed. Retry restoring your place." };
    if (store.event(position.id)) return { type: "found", position };
    if (!history) return { type: "error", message: "Earlier history is unavailable. Retry or go to the latest messages." };
    const covered = !history.hasMore || (position.seq !== undefined && history.before !== null && history.before <= position.seq);
    if (covered) {
      const neighbor = survivingPosition(position, (id) => store.event(id) !== undefined);
      return neighbor ? { type: "found", position: neighbor } : { type: "error", message: "Your saved row is no longer available. Retry or go to the latest messages." };
    }
    if (history.before === null) return { type: "error", message: "Earlier history has no usable cursor. Retry restoring your place." };
    const before = history.before;
    const owned = !history.loading;
    const request = store.loadOlder();
    const cancel = () => {
      if (owned) store.cancelOlder(request);
    };
    signal.addEventListener("abort", cancel, { once: true });
    if (signal.aborted) cancel();
    try {
      await request;
    } finally {
      signal.removeEventListener("abort", cancel);
    }
    if (signal.aborted) return { type: "cancelled" };
    const after = store.get();
    if (!after.ready || !after.history || after.history.cursor !== cursor) return { type: "error", message: "The conversation changed. Retry restoring your place." };
    if (after.history.error) return { type: "error", message: after.history.error };
    if (after.history.hasMore && (after.history.before === null || after.history.before >= before)) return { type: "error", message: "Earlier history did not advance. Retry restoring your place." };
    progress(++pages);
    await yieldFrame(signal);
  }
  return { type: "cancelled" };
}

/** Callback-ref ownership cancels navigation/Latest without a hidden controller or an effect. */
export function RestoringPlace({ store, position, resolved, latest }: { store: SessionStore; position: RowPosition; resolved: (position: RowPosition) => boolean; latest: () => void }) {
  const ready = useSession(store, (snapshot) => snapshot.ready);
  const [attempt, retry] = React.useState(0);
  const [status, setStatus] = React.useState<{ pages: number; error: string | null }>({ pages: 0, error: null });
  const ref = React.useCallback(
    (el: HTMLDivElement | null) => {
      // A deliberate Retry creates a new ref owner and aborts the previous attempt.
      void attempt;
      if (!el || !ready) return;
      const controller = new AbortController();
      setStatus({ pages: 0, error: null });
      void restoreCanonicalPlace(store, position, controller.signal, (pages) => setStatus({ pages, error: null }))
        .then((result) => {
          if (controller.signal.aborted || result.type === "cancelled") return;
          if (result.type === "error") setStatus((status) => ({ ...status, error: result.message }));
          else if (!resolved(result.position)) setStatus((status) => ({ ...status, error: "Your saved row is no longer shown. Retry or go to the latest messages." }));
        })
        .catch(() => {
          if (!controller.signal.aborted) setStatus((status) => ({ ...status, error: "Earlier history could not be loaded. Retry restoring your place." }));
        });
      return () => controller.abort();
    },
    [store, position, resolved, attempt, ready],
  );
  return (
    <div ref={ref} className="hx-restore-place" role="status">
      <p>{status.error ?? `Restoring your saved place${status.pages ? ` · ${status.pages} earlier page${status.pages === 1 ? "" : "s"} loaded` : "…"}`}</p>
      {status.error && (
        <button type="button" className="hx-earlier-steps" onClick={() => retry((n) => n + 1)}>
          Retry restoring place
        </button>
      )}
      <button type="button" className="hx-earlier-steps" onClick={latest}>
        Latest messages
      </button>
    </div>
  );
}
