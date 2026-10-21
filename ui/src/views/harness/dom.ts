/**
 * The agent views' ways of reaching outside React without effects: a listener on the window or the
 * document for as long as a component shows (React subscribes and unsubscribes it as it would a
 * store), clocks that tick for whoever reads them, and conditions held for a while (what turns on
 * and off faster than that never shows). DOM objects that belong to one element (observers, focus,
 * scrolling) are wired with a ref callback that returns its cleanup instead.
 */
import { useCallback, useState, useSyncExternalStore } from "react";

const nothing = () => 0;

type Target = Window | Document | HTMLElement | MediaQueryList;

/**
 * Calls `fn` on `type` events of `target` while the component shows and `on` holds. The latest
 * `fn` is the one called (each render subscribes it again, which is cheap); nothing re-renders.
 */
export function useListener<E extends Event = Event>(target: Target | null, type: string, fn: (e: E) => void, opts: { on?: boolean; capture?: boolean; passive?: boolean } = {}) {
  const { on = true, capture = false, passive } = opts;
  useSyncExternalStore(
    () => {
      if (!on || !target) return () => undefined;
      const h = (e: Event) => fn(e as E);
      target.addEventListener(type, h, { capture, passive });
      return () => target.removeEventListener(type, h, { capture });
    },
    nothing,
    nothing,
  );
}

interface Clock {
  now: number;
  fns: Set<() => void>;
  timer: number | null;
  subscribe: (fn: () => void) => () => void;
}

const clocks = new Map<number, Clock>();

function clockOf(ms: number): Clock {
  const c = clocks.get(ms);
  if (c) return c;
  const clock: Clock = {
    now: Date.now(),
    fns: new Set(),
    timer: null,
    subscribe: (fn) => {
      clock.fns.add(fn);
      if (clock.timer === null) {
        // Read again when it starts, so whoever subscribes first isn't shown a stale time.
        clock.now = Date.now();
        clock.timer = window.setInterval(() => {
          clock.now = Date.now();
          clock.fns.forEach((f) => f());
        }, ms);
      }
      return () => {
        clock.fns.delete(fn);
        if (clock.fns.size === 0 && clock.timer !== null) {
          window.clearInterval(clock.timer);
          clock.timer = null;
        }
      };
    },
  };
  clocks.set(ms, clock);
  return clock;
}

/** The time, re-read every `ms` while shown: one timer per period for every component that reads it. */
export function useClock(ms: number): number {
  const c = clockOf(ms);
  return useSyncExternalStore(
    c.subscribe,
    () => c.now,
    () => c.now,
  );
}

/** Whether moment `at` (performance.now's clock) has come: the component renders again once it does. */
function useCome(at: number): boolean {
  const subscribe = useCallback(
    (notify: () => void) => {
      if (!Number.isFinite(at) || at <= performance.now()) return () => undefined;
      // Once it has come (a timer that fires a moment early waits the rest).
      let timer = 0;
      const check = () => {
        const left = at - performance.now();
        if (left > 0) timer = window.setTimeout(check, left + 1);
        else notify();
      };
      check();
      return () => window.clearTimeout(timer);
    },
    [at],
  );
  return useSyncExternalStore(
    subscribe,
    () => performance.now() >= at,
    () => at === -Infinity,
  );
}

/**
 * Whether `on` has held for `ms`: true once it has, false again as soon as it stops, so what turns
 * on and off faster than that never shows. `already`: it held before this could see it start (a
 * call running for a while when the page opened), so it counts as held at once.
 */
export function useHeld(on: boolean, ms: number, already = false): boolean {
  const [held] = useState(() => ({ since: on ? (already ? -Infinity : performance.now()) : Infinity }));
  if (!on) held.since = Infinity;
  else if (held.since === Infinity) held.since = already ? -Infinity : performance.now();
  const come = useCome(held.since + ms);
  return on && come;
}

/** Whether `on` holds, or held less than `ms` ago. */
export function useRecent(on: boolean, ms: number): boolean {
  const [was] = useState(() => ({ last: on ? Infinity : -Infinity }));
  if (on) was.last = Infinity;
  else if (was.last === Infinity) was.last = performance.now();
  const come = useCome(was.last + ms);
  return on || !come;
}
