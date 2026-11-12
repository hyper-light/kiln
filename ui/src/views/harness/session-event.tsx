/** W21-R02: quotes read their own published event without changing every turn's props. */
import * as React from "react";
import type { SessionStore } from "@/api/harness";

/** The timeline's store stays the same while its events stream. No hidden view is mounted here. */
export const SessionEvents = React.createContext<SessionStore | null>(null);

const idle = () => () => {};

/** The subscription updates this reader only when its target changes (or leaves kept history).
 * The store's index reads the published snapshot, so a quote never gets ahead of its transcript.
 */
export function useSessionEvent(id: string) {
  const store = React.useContext(SessionEvents);
  const read = React.useCallback(() => store?.event(id), [store, id]);
  return React.useSyncExternalStore(store?.subscribe ?? idle, read, read);
}
