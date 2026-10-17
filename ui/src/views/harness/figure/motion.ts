/**
 * Motion, as hyper-light.dev handles it: one pause for every animated figure (remembered), the
 * device's reduced-motion setting always wins, and a figure off screen or in a hidden tab does no
 * work at all.
 */
import * as React from "react";

const KEY = "kiln.motion";
const listeners = new Set<() => void>();
let paused = (() => {
  try {
    return localStorage.getItem(KEY) === "paused";
  } catch {
    return false;
  }
})();

const reducedQuery = () => (typeof window !== "undefined" && window.matchMedia ? window.matchMedia("(prefers-reduced-motion: reduce)") : null);

function mark() {
  if (typeof document === "undefined") return;
  // CSS animations (prism rules, the selected tab's underline, live dots) pause with the figures.
  document.documentElement.dataset.motion = paused || reducedQuery()?.matches ? "paused" : "playing";
}
mark();

export function setMotionPaused(p: boolean) {
  paused = p;
  try {
    localStorage.setItem(KEY, p ? "paused" : "playing");
  } catch {
    /* private mode: just this page */
  }
  mark();
  listeners.forEach((f) => f());
}

function subscribe(f: () => void) {
  listeners.add(f);
  return () => listeners.delete(f);
}
function subscribeReduced(f: () => void) {
  const q = reducedQuery();
  const on = () => {
    mark();
    f();
  };
  q?.addEventListener("change", on);
  return () => q?.removeEventListener("change", on);
}

export function useMotion() {
  const p = React.useSyncExternalStore(subscribe, () => paused);
  const reduced = React.useSyncExternalStore(subscribeReduced, () => reducedQuery()?.matches ?? false);
  return { paused: p || reduced, reduced, toggle: React.useCallback(() => setMotionPaused(!paused), []) };
}

/**
 * One clock per figure: calls `tick(seconds)` up to `fps` times a second while the element is on
 * screen, the tab is visible and motion is on. Time stops while it isn't, and picks up from there.
 */
export function useClock(ref: React.RefObject<Element | null>, running: boolean, tick: (t: number) => void, fps = 30) {
  const elapsed = React.useRef(0);
  const tickRef = React.useRef(tick);
  React.useLayoutEffect(() => {
    tickRef.current = tick;
  });
  React.useEffect(() => {
    const el = ref.current;
    if (!el) return;
    const interval = 1000 / fps;
    let visible = false;
    let raf = 0;
    let prev: number | null = null;
    let last = 0;
    const frame = (now: number) => {
      raf = requestAnimationFrame(frame);
      if (prev !== null) elapsed.current += Math.min(250, Math.max(0, now - prev)) / 1000;
      prev = now;
      if (now - last < interval) return;
      last = now - ((now - last) % interval);
      tickRef.current(elapsed.current);
    };
    const sync = () => {
      cancelAnimationFrame(raf);
      prev = null;
      last = 0;
      if (running && visible && !document.hidden) raf = requestAnimationFrame(frame);
    };
    const io = new IntersectionObserver(
      (entries) => {
        const e = entries.at(-1);
        if (!e) return;
        visible = e.isIntersecting;
        sync();
      },
      { rootMargin: "32px" },
    );
    io.observe(el);
    document.addEventListener("visibilitychange", sync);
    return () => {
      cancelAnimationFrame(raf);
      io.disconnect();
      document.removeEventListener("visibilitychange", sync);
    };
  }, [ref, running, fps]);
  return elapsed;
}
