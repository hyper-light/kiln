import { useCallback, useState, useSyncExternalStore } from "react";
import { ApiError } from "@/api/client";
import { getPref, usePref } from "./prefs";

/* ------------------------------------------------------------------ */
/* Shared ticking clock: one interval for every live timer in the app. */
/* ------------------------------------------------------------------ */

const clockListeners = new Set<() => void>();
let clockNow = Date.now();
let clockTimer: number | null = null;

function subscribeClock(fn: () => void) {
  clockListeners.add(fn);
  if (clockTimer === null) {
    clockTimer = window.setInterval(() => {
      clockNow = Date.now();
      clockListeners.forEach((l) => l());
    }, 1000);
  }
  return () => {
    clockListeners.delete(fn);
    if (clockListeners.size === 0 && clockTimer !== null) {
      window.clearInterval(clockTimer);
      clockTimer = null;
    }
  };
}

/** Current time in ms, re-rendering once a second. */
export function useNow(): number {
  return useSyncExternalStore(subscribeClock, () => clockNow, () => clockNow);
}

/* ------------------------------------------------------------------ */
/* useResource: fetch with abort, refetch, optimistic local mutation.  */
/* Legacy, used by no page now (they read useSuspenseResource,         */
/* lib/resource.ts); kept until it is deleted on main.                 */
/* ------------------------------------------------------------------ */

export function errorMessage(e: unknown): string {
  if (e instanceof ApiError) return e.message;
  if (e instanceof Error) return e.message;
  return String(e);
}

/* ------------------------------------------------------------------ */
/* Local storage-backed state (per-viewer conveniences only).          */
/* ------------------------------------------------------------------ */

/** A preference kept in this browser (see lib/prefs.ts): shared by every reader, kept at once. */
export function useLocalStorage<T>(key: string, initial: T): [T, (v: T) => void] {
  return usePref(key, initial);
}

export function readLocal<T>(key: string, fallback: T): T {
  return getPref(key, fallback);
}

export function useMediaQuery(query: string): boolean {
  return useSyncExternalStore(
    (cb) => {
      const m = window.matchMedia(query);
      m.addEventListener("change", cb);
      return () => m.removeEventListener("change", cb);
    },
    () => window.matchMedia(query).matches,
    () => false,
  );
}

/**
 * For a row that has to scroll sideways: marks which edges hide more (`data-scroll-edge` = start,
 * end or both) so the stylesheet can fade them — a scroller you can see is a scroller. Use the
 * returned callback as the element's ref.
 */
export function useScrollEdges<T extends HTMLElement>(): (el: T | null) => () => void {
  return useCallback((el: T | null) => {
    if (!el) return () => {};
    const update = () => {
      const max = el.scrollWidth - el.clientWidth;
      const start = el.scrollLeft > 2;
      const end = el.scrollLeft < max - 2;
      const v = max <= 2 ? "" : start && end ? "both" : start ? "start" : end ? "end" : "";
      if (v) el.dataset.scrollEdge = v;
      else delete el.dataset.scrollEdge;
    };
    update();
    el.addEventListener("scroll", update, { passive: true });
    const ro = new ResizeObserver(update);
    ro.observe(el);
    for (const c of Array.from(el.children)) ro.observe(c);
    return () => {
      el.removeEventListener("scroll", update);
      ro.disconnect();
    };
  }, []);
}

/**
 * An element's width, kept as it changes: `[ref, width]`. Measured as the ref attaches (before
 * paint), so a layout chosen by width never shows a frame of the wrong one; 0 until then.
 */
export function useElementWidth<T extends HTMLElement>(): [(el: T | null) => (() => void) | undefined, number] {
  const [width, setWidth] = useState(0);
  const ref = useCallback((el: T | null) => {
    if (!el) return;
    setWidth(el.clientWidth);
    const ro = new ResizeObserver(() => setWidth(el.clientWidth));
    ro.observe(el);
    return () => ro.disconnect();
  }, []);
  return [ref, width];
}
