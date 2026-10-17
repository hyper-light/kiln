import { useCallback, useEffect, useRef, useState, useSyncExternalStore } from "react";
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
/* Legacy: the agent console's views still use it until they move to   */
/* useSuspenseResource (lib/resource.ts) like every other page has.    */
/* ------------------------------------------------------------------ */

export interface Resource<T> {
  data: T | undefined;
  error: Error | null;
  loading: boolean;
  reload: () => void;
  setData: (fn: (prev: T | undefined) => T | undefined) => void;
}

export function useResource<T>(
  fetcher: (signal: AbortSignal) => Promise<T>,
  deps: React.DependencyList,
  opts: { pollMs?: number } = {},
): Resource<T> {
  const [data, setDataState] = useState<T | undefined>(undefined);
  const [error, setError] = useState<Error | null>(null);
  const [loading, setLoading] = useState(true);
  const [nonce, setNonce] = useState(0);
  const fetcherRef = useRef(fetcher);
  useEffect(() => {
    fetcherRef.current = fetcher;
  });

  useEffect(() => {
    const ctl = new AbortController();
    setLoading(true);
    fetcherRef
      .current(ctl.signal)
      .then((d) => {
        if (ctl.signal.aborted) return;
        setDataState(d);
        setError(null);
      })
      .catch((e: unknown) => {
        if (ctl.signal.aborted) return;
        if (e instanceof DOMException && e.name === "AbortError") return;
        setError(e instanceof Error ? e : new Error(String(e)));
      })
      .finally(() => {
        if (!ctl.signal.aborted) setLoading(false);
      });
    return () => ctl.abort();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [...deps, nonce]);

  useEffect(() => {
    if (!opts.pollMs) return;
    const t = window.setInterval(() => {
      if (!document.hidden) setNonce((n) => n + 1);
    }, opts.pollMs);
    return () => window.clearInterval(t);
  }, [opts.pollMs]);

  const reload = useCallback(() => setNonce((n) => n + 1), []);
  const setData = useCallback((fn: (prev: T | undefined) => T | undefined) => setDataState(fn), []);
  return { data, error, loading, reload, setData };
}

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
 * Legacy: pages render a `<title>` element (React hoists it into the head). Kept for the agent
 * console's views until they do the same.
 */
export function useDocumentTitle(title: string) {
  useEffect(() => {
    document.title = title ? `${title} · kiln` : "kiln";
  }, [title]);
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
