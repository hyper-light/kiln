/**
 * The agent views' ways of reaching outside React without effects: a listener on the window or the
 * document for as long as a component shows (React subscribes and unsubscribes it as it would a
 * store), and clocks that tick for whoever reads them. DOM objects that belong to one element
 * (observers, focus, scrolling) are wired with a ref callback that returns its cleanup instead.
 */
import { useSyncExternalStore } from "react";

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
  let c = clocks.get(ms);
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
