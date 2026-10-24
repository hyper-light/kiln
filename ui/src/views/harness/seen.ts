/**
 * What you've already looked at: for each agent, when you last had it open, kept for you by the
 * server (notices.ts), the same whichever device, browser or kiln address you look from. Mission
 * control uses it to tell work that finished and wants your review from agents simply standing by.
 */
import * as React from "react";
import { markSeen, useMarks } from "./notices";

/** When you last saw each agent (unix ms), by its id. */
export function useSeen(): Record<string, number> {
  const marks = useMarks();
  return React.useMemo(() => {
    const out: Record<string, number> = {};
    for (const m of marks.values()) if (m.how === "seen" && m.key.startsWith("seen:")) out[m.key.slice(5)] = m.at;
    return out;
  }, [marks]);
}

const nothing = () => 0;

/**
 * While an agent's page is open, everything it does counts as seen: now, every few seconds while
 * the tab is shown, and on leaving. (Subscribed to as a store is, so it runs once per agent shown.)
 */
export function useMarkSeen(id: string) {
  const watch = React.useCallback(() => {
    if (!id) return () => undefined;
    const mark = () => {
      if (!document.hidden) markSeen(id);
    };
    mark();
    const t = window.setInterval(mark, 10_000);
    document.addEventListener("visibilitychange", mark);
    return () => {
      window.clearInterval(t);
      document.removeEventListener("visibilitychange", mark);
      markSeen(id, true);
    };
  }, [id]);
  React.useSyncExternalStore(watch, nothing, nothing);
}
