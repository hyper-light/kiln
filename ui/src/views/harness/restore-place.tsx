import type { OlderPage, SessionStore } from "@/api/harness";
import { survivingPosition, type RowPosition } from "./row-window";

type RestoreStore = Pick<SessionStore, "get" | "event" | "loadOlder" | "cancelOlder">;
/** A place restored reads the most the server gives at once (api.rs `conversation_events_for`), so one far back is a read
 * or two rather than one per hundred events (plan 01a1cc09, step 3). */
const RESTORE_PAGE: OlderPage = { limit: 1000, bytes: 1024 * 1024 };
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
    const request = store.loadOlder(RESTORE_PAGE);
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
