/**
 * What you've already looked at: for each agent, when you last had it open. Mission control uses it
 * to tell work that finished and wants your review from agents simply standing by. Per browser.
 */
import * as React from "react";

const KEY = "kiln.agents.seen";
const listeners = new Set<() => void>();
let cache: Record<string, number> | null = null;

function read(): Record<string, number> {
  if (cache) return cache;
  try {
    const v: unknown = JSON.parse(localStorage.getItem(KEY) ?? "{}");
    cache = v && typeof v === "object" ? (v as Record<string, number>) : {};
  } catch {
    cache = {};
  }
  return cache;
}

function write(m: Record<string, number>) {
  // Keep the most recent few hundred.
  const entries = Object.entries(m);
  const kept = entries.length > 400 ? Object.fromEntries(entries.sort((a, b) => b[1] - a[1]).slice(0, 300)) : m;
  cache = kept;
  try {
    localStorage.setItem(KEY, JSON.stringify(kept));
  } catch {
    /* private mode: this tab only */
  }
  listeners.forEach((f) => f());
}

export function markSeen(id: string, at = Date.now()) {
  const m = read();
  if ((m[id] ?? 0) >= at) return;
  write({ ...m, [id]: at });
}

function subscribe(f: () => void) {
  listeners.add(f);
  const onStorage = (e: StorageEvent) => {
    if (e.key !== KEY) return;
    cache = null;
    f();
  };
  window.addEventListener("storage", onStorage);
  return () => {
    listeners.delete(f);
    window.removeEventListener("storage", onStorage);
  };
}

export function useSeen(): Record<string, number> {
  return React.useSyncExternalStore(subscribe, read, () => ({}));
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
      markSeen(id);
    };
  }, [id]);
  React.useSyncExternalStore(watch, nothing, nothing);
}
