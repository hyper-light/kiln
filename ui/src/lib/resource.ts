/**
 * Server data, read through Suspense. The first read of a key suspends, so the nearest
 * `<Suspense>` (see `Loadable`) shows its loader instead of an empty page. Every later read gets
 * the last data at once while it refreshes in the background, so a page that comes back is there
 * immediately and nothing jumps when the new data lands. A key refreshes:
 * - when it's shown again after being away;
 * - when the server says it changed (`on`: event types from /api/events);
 * - on its interval while shown (`pollMs`; not while the tab is hidden);
 * - after the event stream reconnects (the server restarted);
 * - on `invalidate(prefix)`, after a change made here.
 * No effects: a key is subscribed to through useSyncExternalStore, and its first load starts
 * while rendering, cached, as Suspense expects.
 */
import { use, useSyncExternalStore } from "react";
import { events } from "@/api/events";

type Fetcher<T> = (signal: AbortSignal) => Promise<T>;

export interface ResourceOptions {
  /** Server event types (`ServerEvent["type"]`) after which it's out of date. */
  on?: readonly string[];
  /** Refresh this often while it's shown. */
  pollMs?: number;
}

export interface SuspenseResource<T> {
  data: T;
  /** A refresh is on its way (the data shown is the last there was). */
  refreshing: boolean;
  reload: () => void;
  /** Change the data here, at once (after a change the server will confirm). */
  setData: (fn: (prev: T) => T) => void;
}

interface Snap {
  data: unknown;
  has: boolean;
  error: unknown;
  failed: boolean;
  refreshing: boolean;
}

interface Entry {
  key: string;
  fetcher: Fetcher<unknown>;
  opts: ResourceOptions;
  snap: Snap;
  promise: Promise<void> | null;
  /** Asked to refresh while a load was on its way: load once more after it. */
  again: boolean;
  /** Out of date while not shown: refreshed when it's shown again. */
  stale: boolean;
  listeners: Set<() => void>;
  poll: number | null;
  used: number;
}

/**
 * Already settled, for React: `use()` reads its status and doesn't wait. A read must call `use()`
 * on every render, not only while loading: React replays the render that suspended and needs the
 * same calls (react.dev/warnings/conditional-use-of-use).
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
      again: false,
      stale: false,
      listeners: new Set(),
      poll: null,
      used: Date.now(),
    };
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
  const p = e
    .fetcher(new AbortController().signal)
    .then(
      (data) => set(e, { data, has: true, error: undefined, failed: false, refreshing: false }),
      (error: unknown) => {
        // A refresh that failed leaves the data that's shown; a first load that failed says so.
        if (e.snap.has) set(e, { refreshing: false });
        else set(e, { error, failed: true, refreshing: false });
      },
    )
    .finally(() => {
      e.promise = null;
      if (e.again) {
        e.again = false;
        refresh(e);
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

function subscribe(e: Entry, l: () => void) {
  e.listeners.add(l);
  e.used = Date.now();
  if (e.listeners.size === 1) {
    if (e.stale && !e.promise) void load(e);
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
export function useSuspenseResource<T>(key: string, fetcher: Fetcher<T>, opts: ResourceOptions = {}): SuspenseResource<T> {
  const e = entry(key, fetcher as Fetcher<unknown>, opts);
  const snap = useSyncExternalStore(
    (l) => subscribe(e, l),
    () => e.snap,
    () => e.snap,
  );
  if (!snap.has && snap.failed) throw snap.error;
  use(snap.has ? SETTLED : (e.promise ?? load(e)));
  return {
    data: snap.data as T,
    refreshing: snap.refreshing,
    reload: () => void load(e),
    setData: (fn) => set(e, { data: fn(e.snap.data as T), has: true }),
  };
}

/**
 * Starts loading `key` now, without suspending. A section that reads several keys calls this for
 * each of them first, so their fetches run at once rather than one after another (each read
 * suspends before the next one starts).
 */
export function preload<T>(key: string, fetcher: Fetcher<T>, opts: ResourceOptions = {}) {
  const e = entry(key, fetcher as Fetcher<unknown>, opts);
  if (!e.snap.has && !e.snap.failed && !e.promise) void load(e);
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
  for (const e of entries.values()) if (e.opts.on?.includes(ev.type)) refresh(e);
});
// The server restarted, or the network came back: what's shown may be out of date.
events.onReopen(() => invalidate());
