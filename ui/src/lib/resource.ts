/**
 * Server data, read through Suspense. The first read of a key suspends, so the nearest
 * `<Suspense>` (see `Loadable`) shows its loader instead of an empty page. Every later read gets
 * the last data at once while it refreshes in the background, so a page that comes back is there
 * immediately and nothing jumps when the new data lands. A key refreshes:
 * - when it's shown again after being away;
 * - when the server says it changed (`on`: event types from /api/events), at most once a second;
 *   or, where the event itself says what changed (`apply`), with it, without loading anything;
 * - on its interval while shown (`pollMs`; not while the tab is hidden);
 * - after the event stream reconnects (the server restarted);
 * - on `invalidate(prefix)`, after a change made here.
 * No effects: a key is subscribed to through useSyncExternalStore, and its first load starts
 * while rendering, cached, as Suspense expects.
 */
import { use, useSyncExternalStore } from "react";
import { events } from "@/api/events";
import type { ServerEvent } from "@/api/types";

type Fetcher<T> = (signal: AbortSignal) => Promise<T>;

export interface ResourceOptions<T = unknown> {
  /** Server event types (`ServerEvent["type"]`) after which it's out of date. */
  on?: readonly string[];
  /**
   * The data brought up to date with one of the `on` events, from what the event says (undefined:
   * it can't be, so it's loaded again). An event that carries the whole of what changed then costs
   * no load: many agents at work send a stream of them, and a list loaded again for each would
   * fill a slow link.
   */
  apply?(data: T, ev: ServerEvent): T | undefined;
  /** Refresh this often while it's shown. */
  pollMs?: number;
}

/** A load asked for by the server's events waits this long after the last load: a burst of them loads once. */
const EVENT_GAP_MS = 1000;
/** Events applied to a key's data (`apply`) are gathered and applied together at most this often. */
const APPLY_MS = 250;

export interface SuspenseResource<T> {
  data: T;
  /** A refresh is on its way (the data shown is the last there was). */
  refreshing: boolean;
  reload: () => void;
  /** Change the data here, at once (after a change the server will confirm). */
  setData: (fn: (prev: T) => T) => void;
  /** The last refresh failed, and why: the data shown is the last there was. */
  refreshError?: unknown;
}

interface Snap {
  data: unknown;
  has: boolean;
  error: unknown;
  failed: boolean;
  refreshing: boolean;
  refreshError?: unknown;
}

interface Entry {
  key: string;
  fetcher: Fetcher<unknown>;
  opts: ResourceOptions;
  snap: Snap;
  promise: Promise<void> | null;
  /** What its first read suspended on. Reads after it hand React that same promise (settled), so a
   * render React replays after suspending gets back the promise it waited for. */
  read: Promise<void> | null;
  /** Asked to refresh while a load was on its way: load once more after it. */
  again: boolean;
  /** The server's events said it changed while a load was on its way: load again after it, in a while. */
  later: boolean;
  /** When its last load ended (ms), and a load waiting on `EVENT_GAP_MS`. */
  settled: number;
  soon: number;
  /** Events to apply to its data, and when they will be. */
  news: ServerEvent[];
  applying: number;
  /** Out of date while not shown: refreshed when it's shown again. */
  stale: boolean;
  listeners: Set<() => void>;
  /** Its subscribe function, one per key: a new one each render would make React unsubscribe and
   * subscribe again after every render, and each new subscription refresh it. */
  sub: (l: () => void) => () => void;
  poll: number | null;
  used: number;
}

/**
 * Already settled, for React: `use()` reads its status and doesn't wait. A read must call `use()`
 * on every render, not only while loading: React replays the render that suspended and needs the
 * same calls (react.dev/warnings/conditional-use-of-use). For data that came without a load
 * (`setData`); a key that was loaded hands back the promise its first read waited on.
 */
const SETTLED: Promise<void> = Object.assign(Promise.resolve(), { status: "fulfilled", value: undefined });

/** At most this many keys are kept once nothing shows them. */
const KEEP = 200;
const entries = new Map<string, Entry>();

function entry(key: string, fetcher: Fetcher<unknown>, opts: ResourceOptions): Entry {
  let e = entries.get(key);
  if (!e) {
    e = {
      key,
      fetcher,
      opts,
      snap: { data: undefined, has: false, error: undefined, failed: false, refreshing: false },
      promise: null,
      read: null,
      again: false,
      later: false,
      settled: 0,
      soon: 0,
      news: [],
      applying: 0,
      stale: false,
      listeners: new Set(),
      sub: () => () => undefined,
      poll: null,
      used: Date.now(),
    };
    const made = e;
    e.sub = (l) => subscribe(made, l);
    entries.set(key, e);
    prune();
  }
  e.fetcher = fetcher;
  e.opts = opts;
  return e;
}

function prune() {
  if (entries.size <= KEEP) return;
  const idle = [...entries.values()].filter((e) => e.listeners.size === 0 && !e.promise).sort((a, b) => a.used - b.used);
  for (const e of idle.slice(0, entries.size - KEEP)) entries.delete(e.key);
}

function set(e: Entry, patch: Partial<Snap>) {
  e.snap = { ...e.snap, ...patch };
  e.listeners.forEach((l) => l());
}

/**
 * Loads `e`. One load at a time per key: a refresh asked for while one is on its way makes one more
 * load after it (however many were asked for), so a key whose events come faster than its fetch
 * still refreshes, instead of each refresh cancelling the last.
 */
function load(e: Entry): Promise<void> {
  if (e.promise) {
    e.again = true;
    return e.promise;
  }
  e.stale = false;
  e.again = false;
  const p: Promise<void> = e
    .fetcher(new AbortController().signal)
    .then(
      (data) => {
        // Settled for React before anyone hears of the data: the render it replays reads on at once.
        Object.assign(p, { status: "fulfilled", value: undefined });
        set(e, { data, has: true, error: undefined, failed: false, refreshing: false, refreshError: undefined });
      },
      (error: unknown) => {
        // A refresh that failed leaves the data that's shown (and says why); a first load that
        // failed says so.
        if (e.snap.has) set(e, { refreshing: false, refreshError: error });
        else set(e, { error, failed: true, refreshing: false });
      },
    )
    .finally(() => {
      e.promise = null;
      e.settled = Date.now();
      if (e.again) {
        e.again = false;
        e.later = false;
        refresh(e);
      } else if (e.later) {
        e.later = false;
        soon(e);
      }
    });
  e.promise = p;
  if (e.snap.has) set(e, { refreshing: true });
  return p;
}

function refresh(e: Entry) {
  if (e.listeners.size > 0) void load(e);
  else e.stale = true;
}

/** Refresh for the server's events: not sooner than `EVENT_GAP_MS` after the last load. */
function soon(e: Entry) {
  if (e.listeners.size === 0) {
    e.stale = true;
    return;
  }
  if (e.promise) {
    e.later = true;
    return;
  }
  if (e.soon) return;
  const wait = e.settled + EVENT_GAP_MS - Date.now();
  if (wait <= 0) {
    void load(e);
    return;
  }
  e.soon = window.setTimeout(() => {
    e.soon = 0;
    soon(e);
  }, wait);
}

/** Bring `e`'s data up to date with the server's events, gathered (`apply`). */
function take(e: Entry, ev: ServerEvent) {
  e.news.push(ev);
  if (e.applying) return;
  e.applying = window.setTimeout(() => {
    e.applying = 0;
    const news = e.news;
    e.news = [];
    if (e.listeners.size === 0) {
      e.stale = true;
      return;
    }
    let data = e.snap.data;
    for (const n of news) {
      const next = e.snap.has ? e.opts.apply?.(data, n) : undefined;
      if (next === undefined) {
        soon(e);
        return;
      }
      data = next;
    }
    if (data !== e.snap.data) set(e, { data });
    // A load on its way read the data before these: once it lands, load again.
    if (e.promise) e.later = true;
  }, APPLY_MS);
}

function subscribe(e: Entry, l: () => void) {
  e.listeners.add(l);
  e.used = Date.now();
  if (e.listeners.size === 1) {
    if (e.stale) {
      if (e.promise) e.later = true;
      else void load(e);
    }
    if (e.opts.pollMs && e.poll === null) {
      e.poll = window.setInterval(() => {
        if (!document.hidden && !e.promise) void load(e);
      }, e.opts.pollMs);
    }
  }
  return () => {
    e.listeners.delete(l);
    if (e.listeners.size === 0) {
      if (e.poll !== null) window.clearInterval(e.poll);
      e.poll = null;
      e.stale = true;
      e.used = Date.now();
    }
  };
}

/**
 * The data under `key`, loaded by `fetcher`: suspends until the first data is in (wrap the
 * section in `Loadable`), then always returns data. Keys name what they load ("runners",
 * "front-door/adacachy"), so `invalidate("front-door")` refreshes every one of them.
 */
export function useSuspenseResource<T>(key: string, fetcher: Fetcher<T>, opts: ResourceOptions<T> = {}): SuspenseResource<T> {
  const e = entry(key, fetcher as Fetcher<unknown>, opts);
  const snap = useSyncExternalStore(
    e.sub,
    () => e.snap,
    () => e.snap,
  );
  if (!snap.has && snap.failed) throw snap.error;
  use(snap.has ? (e.read ?? SETTLED) : (e.read = e.promise ?? load(e)));
  return {
    data: snap.data as T,
    refreshing: snap.refreshing,
    reload: () => void load(e),
    setData: (fn) => set(e, { data: fn(e.snap.data as T), has: true }),
    refreshError: snap.refreshError,
  };
}

/**
 * Starts loading `key` now, without suspending. A section that reads several keys calls this for
 * each of them first, so their fetches run at once rather than one after another (each read
 * suspends before the next one starts).
 */
export function preload<T>(key: string, fetcher: Fetcher<T>, opts: ResourceOptions<T> = {}) {
  const e = entry(key, fetcher as Fetcher<unknown>, opts);
  if (!e.snap.has && !e.snap.failed && !e.promise) void load(e);
}

/**
 * `preload`, settled once its first data is in (or its first load failed). A route's loader awaits
 * it, so its page renders with its data and never shows its fallback: once a fallback is shown,
 * React reveals what replaces it no sooner than 300 ms on, however soon the data came.
 */
export function loaded<T>(key: string, fetcher: Fetcher<T>, opts: ResourceOptions<T> = {}): Promise<void> {
  const e = entry(key, fetcher as Fetcher<unknown>, opts);
  if (e.snap.has || e.snap.failed) return Promise.resolve();
  return e.promise ?? load(e);
}

/** Refreshes every key that starts with `prefix` (all of them for ""). */
export function invalidate(prefix = "") {
  for (const e of entries.values()) if (e.key.startsWith(prefix)) refresh(e);
}

/** Lets the keys whose first load failed try again (`Loadable`'s Try again). */
export function retryFailed() {
  for (const e of entries.values()) if (e.snap.failed) e.snap = { ...e.snap, failed: false, error: undefined };
}

events.on((ev) => {
  if (ev.type === "resync") {
    for (const e of entries.values()) soon(e);
    return;
  }
  for (const e of entries.values()) {
    if (!e.opts.on?.includes(ev.type)) continue;
    if (e.listeners.size === 0) e.stale = true;
    else if (e.opts.apply) take(e, ev);
    else soon(e);
  }
});
// The server restarted, or the network came back: what's shown may be out of date.
events.onReopen(() => invalidate());
