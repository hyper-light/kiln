/**
 * Server data, read through Suspense. The first read of a key suspends, so the nearest
 * `<Suspense>` (see `Loadable`) shows its loader instead of an empty page. Every later read gets
 * the last data at once while it refreshes in the background, so a page that comes back is there
 * immediately and nothing jumps when the new data lands. A key refreshes:
 * - when it's shown again after being away;
 * - when the server says it changed (`on`: event types from /api/events), at most once a second;
 *   or, where the event itself says what changed (`apply`), with it, without loading anything;
 * - on its interval while shown (`pollMs`; not while the tab is hidden);
 * - after the event stream reconnects without taking up where it was (`onMissed`: the server
 *   restarted, or it was away too long): a reconnect that resumes is sent what it missed instead;
 * - on `invalidate(prefix)`, after a change made here.
 * The event stream is asked only for the `on` types of the keys that need them (`interested`).
 * No effects: a key is subscribed to through useSyncExternalStore, and its first load starts
 * while rendering, cached, as Suspense expects.
 */
import { use, useSyncExternalStore } from "react";
import { events } from "@/api/events";
import { onAuthChange } from "@/api/client";
import type { ServerEvent } from "@/api/types";

const COHORT = Symbol("resource cohort");

export type ResourceFetcher<T> = (signal: AbortSignal) => Promise<T | ResourceCohort<T>>;
type Fetcher<T> = ResourceFetcher<T>;

export interface ResourceSource<T> {
  key: string;
  fetch: ResourceFetcher<T>;
  opts?: ResourceOptions<T>;
}

interface Publication {
  source: ResourceSource<unknown>;
  data: unknown;
}

export interface ResourceCohort<T> {
  readonly [COHORT]: true;
  readonly data: T;
  readonly records: readonly Publication[];
}

/** W21-R02: related records land in their canonical owners, before any reader is notified. */
export function resourceCohort<T>(data: T, records: readonly Publication[]): ResourceCohort<T> {
  return { [COHORT]: true, data, records };
}

export function resourceRecord<T>(source: ResourceSource<T>, data: T): Publication {
  return { source: source as ResourceSource<unknown>, data };
}

function isCohort(value: unknown): value is ResourceCohort<unknown> {
  return typeof value === "object" && value !== null && COHORT in value;
}

export interface ResourceOptions<T = unknown> {
  /** Server event types (`ServerEvent["type"]`) after which it's out of date. */
  on?: readonly string[];
  /** Ignore unrelated events before they can invalidate an owner or an in-flight cohort. */
  relevant?(ev: ServerEvent): boolean;
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
  /**
   * What a press shows at once, before its request is answered (W21-R15): `setData`, and made again
   * over what a load already on its way brings, which was read before it.
   */
  change: (fn: (prev: T) => T) => void;
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
  controller: AbortController | null;
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
  /** Changes presses made (`change`) while the load on its way was out: made again over what it brings. */
  made: ((data: unknown) => unknown)[];
  /** Out of date while not shown: refreshed when it's shown again. */
  stale: boolean;
  listeners: Set<() => void>;
  /** Its subscribe function, one per key: a new one each render would make React unsubscribe and
   * subscribe again after every render, and each new subscription refresh it. */
  sub: (l: () => void) => () => void;
  poll: number | null;
  used: number;
  preparations: Set<symbol>;
  written: number;
  invalidated: number;
  preparationOnly: boolean;
}

/**
 * Already settled, for React: `use()` reads its status and doesn't wait. A read must call `use()`
 * on every render, not only while loading: React replays the render that suspended and needs the
 * same calls (react.dev/warnings/conditional-use-of-use). For data that came without a load
 * (`setData`); a key that was loaded hands back the promise its first read waited on.
 */
const SETTLED: Promise<void> = Object.assign(Promise.resolve(), {
  status: "fulfilled",
  value: undefined,
});

/** At most this many keys are kept once nothing shows them. */
const KEEP = 200;
const entries = new Map<string, Entry>();
let authEpoch = 0;
let written = 0;

function entry(key: string, fetcher: Fetcher<unknown>, opts: ResourceOptions): Entry {
  let e = entries.get(key);
  if (!e) {
    e = {
      key,
      fetcher,
      opts,
      snap: {
        data: undefined,
        has: false,
        error: undefined,
        failed: false,
        refreshing: false,
      },
      promise: null,
      controller: null,
      read: null,
      again: false,
      later: false,
      settled: 0,
      soon: 0,
      news: [],
      applying: 0,
      made: [],
      stale: false,
      listeners: new Set(),
      sub: () => () => undefined,
      poll: null,
      used: Date.now(),
      preparations: new Set(),
      written: 0,
      invalidated: 0,
      preparationOnly: false,
    };
    const made = e;
    e.sub = (l) => subscribe(made, l);
    entries.set(key, e);
    prune();
    events.changed();
  }
  e.fetcher = fetcher;
  e.opts = opts;
  return e;
}

/**
 * `e` needs the server's events of its `on` types: it's shown, a load of it is on its way, or it
 * holds data not yet known to be out of date (loaded ahead of being shown). One that's out of date
 * and not shown loads again when it's shown, so it needs none.
 */
function interested(e: Entry): boolean {
  return Boolean(e.opts.on?.length) && (e.listeners.size > 0 || e.promise !== null || (e.snap.has && !e.stale));
}

/** The event types the keys need now (`interested`). */
export function wantedTypes(): Set<string> {
  const all = new Set<string>();
  for (const e of entries.values()) if (interested(e)) for (const t of e.opts.on ?? []) all.add(t);
  return all;
}

/** `e` is out of date: the stream needn't bring it events till it's shown again. */
function stale(e: Entry) {
  if (e.stale) return;
  e.stale = true;
  events.changed();
}

function prune() {
  if (entries.size <= KEEP) return;
  const idle = [...entries.values()].filter((e) => e.listeners.size === 0 && e.preparations.size === 0 && !e.promise).sort((a, b) => a.used - b.used);
  for (const e of idle.slice(0, entries.size - KEEP)) entries.delete(e.key);
}

function set(e: Entry, patch: Partial<Snap>) {
  if ("data" in patch) e.written = ++written;
  e.snap = { ...e.snap, ...patch };
  e.listeners.forEach((l) => l());
}

/**
 * How many keys' first loads have ended (with data or failed). `Loadable` re-renders as it
 * changes, so a section that waited on one shows as it lands: React would hold it back until
 * 300 ms after the loader showed (its throttle on revealing what suspended), and the loader
 * itself shows only after a moment, so a fast load would show nothing and then wait.
 */
let landed = 0;
const landing = new Set<() => void>();
const subscribeLanded = (l: () => void) => {
  landing.add(l);
  return () => void landing.delete(l);
};

function land() {
  landed++;
  landing.forEach((l) => l());
}

/** `landed`, for `Loadable`. */
export function useLanded(): number {
  return useSyncExternalStore(
    subscribeLanded,
    () => landed,
    () => landed,
  );
}

/**
 * Loads `e`. One load at a time per key: a refresh asked for while one is on its way makes one more
 * load after it (however many were asked for), so a key whose events come faster than its fetch
 * still refreshes, instead of each refresh cancelling the last.
 */
function load(e: Entry, preparation = false): Promise<void> {
  if (e.promise) {
    e.again = true;
    return e.promise;
  }
  e.stale = false;
  e.again = false;
  e.preparationOnly = preparation;
  e.made = [];
  const at = authEpoch;
  const fetcher = e.fetcher;
  const controller = new AbortController();
  e.controller = controller;
  const current = () => at === authEpoch && e.controller === controller && !controller.signal.aborted;
  const fetch = async () => {
    for (let attempt = 0; current(); attempt++) {
      const before = written;
      const value = await fetcher(controller.signal);
      if (!current()) return;
      const records = isCohort(value) ? value.records.map(({ source, data }) => ({ e: entry(source.key, source.fetch, source.opts ?? {}), data })) : [];
      const keys = new Set(records.map((record) => record.e.key));
      if (keys.has(e.key) || keys.size !== records.length) throw new Error("A resource cohort must name each canonical owner once");
      // A concurrent canonical read wins. Read the cohort again after it, instead of mixing revisions.
      const crossed = isCohort(value) && (e.written > before || e.invalidated > before || records.some((record) => record.e.promise || record.e.written > before || record.e.invalidated > before));
      if (crossed) {
        if (attempt >= 2) throw new Error("The related records changed while loading. Try again.");
        await Promise.all(records.flatMap((record) => (record.e.promise ? [record.e.promise] : [])));
        // Only presses made after the next request starts stand over that request.
        e.made = [];
        continue;
      }
      const publications = [{ e, data: isCohort(value) ? value.data : value }, ...records];
      Object.assign(p, { status: "fulfilled", value: undefined });
      let first = false;
      // One auth/request check governs the whole synchronous commit; notifications follow every write.
      for (const record of publications) {
        first ||= !record.e.snap.has;
        record.e.written = ++written;
        record.e.read ??= SETTLED;
        record.e.stale = record.e.invalidated > before;
        record.e.settled = Date.now();
        const made = record.e.made;
        record.e.made = [];
        record.e.snap = { data: made.reduce((data, fn) => fn(data), record.data), has: true, error: undefined, failed: false, refreshing: false, refreshError: undefined };
      }
      for (const record of publications) record.e.listeners.forEach((listener) => listener());
      if (first) land();
      return;
    }
  };
  const p: Promise<void> = fetch()
    .catch((error: unknown) => {
      if (!current()) return;
      if (e.snap.has) set(e, { refreshing: false, refreshError: error });
      else {
        Object.assign(p, { status: "fulfilled", value: undefined });
        set(e, { error, failed: true, refreshing: false });
        land();
      }
    })
    .finally(() => {
      if (e.controller !== controller) return;
      e.controller = null;
      e.promise = null;
      if (at !== authEpoch) return;
      e.settled = Date.now();
      events.changed();
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
  events.changed();
  if (e.snap.has) set(e, { refreshing: true });
  return p;
}

function markStale(e: Entry) {
  e.invalidated = ++written;
  stale(e);
  if (e.promise) e.later = true;
}

function refresh(e: Entry) {
  markStale(e);
  if (e.listeners.size > 0 || e.preparations.size > 0) void load(e);
}

/** Refresh for the server's events: not sooner than `EVENT_GAP_MS` after the last load. */
function soon(e: Entry) {
  markStale(e);
  if (e.listeners.size === 0 && e.preparations.size === 0) return;
  if (e.promise) {
    e.later = true;
    return;
  }
  if (e.preparations.size > 0 && e.listeners.size === 0) {
    void load(e, true);
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
  markStale(e);
  e.news.push(ev);
  if (e.applying) return;
  e.applying = window.setTimeout(() => {
    e.applying = 0;
    const news = e.news;
    e.news = [];
    if (e.listeners.size === 0) {
      stale(e);
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
    events.changed();
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
      stale(e);
      events.changed();
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
  e.preparationOnly = false;
  if (!snap.has && snap.failed) throw snap.error;
  use(snap.has ? (e.read ?? SETTLED) : (e.read = e.promise ?? load(e)));
  return {
    data: snap.data as T,
    refreshing: snap.refreshing,
    reload: () => void load(e),
    setData: (fn) => set(e, { data: fn(e.snap.data as T), has: true }),
    change: (fn) => changed(e, fn as (prev: unknown) => unknown),
    refreshError: snap.refreshError,
  };
}

/** `change`, of entry `e`. */
function changed(e: Entry, fn: (prev: unknown) => unknown) {
  if (!e.snap.has) return;
  if (e.promise) {
    e.made.push(fn);
    // W21-R15: this press stands over the already-out read. It is not a competing
    // authoritative publication that should retry that read against a later snapshot.
    e.snap = { ...e.snap, data: fn(e.snap.data) };
    e.listeners.forEach((listener) => listener());
  } else set(e, { data: fn(e.snap.data) });
}

/**
 * `key`'s data, when it was loaded less than `ms` ago and nothing since says it's out of date: a
 * key that reads what another already holds takes it instead of asking the server again.
 */
export function recent<T>(key: string, ms: number): T | undefined {
  const e = entries.get(key);
  return e && e.snap.has && !e.stale && !e.promise && Date.now() - e.settled < ms ? (e.snap.data as T) : undefined;
}

/** The current canonical value, without starting another read or creating another owner. */
export function resourceData<T>(source: ResourceSource<T>): T | undefined {
  const e = entries.get(source.key);
  return e?.snap.has ? (e.snap.data as T) : undefined;
}

/** W21-R02: actual navigation owns a canonical read until it settles or is canceled. Shared/view reads survive. */
export function acquireResource<T>(source: ResourceSource<T>, signal?: AbortSignal): Promise<T> {
  if (signal?.aborted) return Promise.reject(new DOMException("Navigation canceled", "AbortError"));
  const e = entry(source.key, source.fetch as Fetcher<unknown>, source.opts ?? {});
  const epoch = authEpoch;
  const owner = Symbol();
  e.preparations.add(owner);
  return new Promise<T>((resolve, reject) => {
    const release = () => {
      e.preparations.delete(owner);
      signal?.removeEventListener("abort", abort);
    };
    const abort = () => {
      release();
      if (e.preparations.size === 0 && e.listeners.size === 0 && e.preparationOnly && e.controller) {
        const controller = e.controller;
        e.controller = null;
        e.promise = null;
        e.read = e.snap.has ? SETTLED : null;
        e.stale = true;
        e.again = false;
        e.later = false;
        controller.abort();
        set(e, { refreshing: false });
      }
      reject(new DOMException("Navigation canceled", "AbortError"));
    };
    signal?.addEventListener("abort", abort, { once: true });
    const pending = e.promise ?? (!e.snap.has || e.stale ? load(e, true) : Promise.resolve());
    void pending.then(() => {
      release();
      if (signal?.aborted || epoch !== authEpoch) reject(new DOMException("Navigation canceled", "AbortError"));
      else if (e.snap.failed) reject(e.snap.error);
      else if (e.snap.has) resolve(e.snap.data as T);
      else reject(new DOMException("Navigation canceled", "AbortError"));
    });
  });
}

/**
 * Starts loading `key` now, without suspending. A section that reads several keys calls this for
 * each of them first, so their fetches run at once rather than one after another (each read
 * suspends before the next one starts).
 */
export function preload<T>(key: string, fetcher: Fetcher<T>, opts: ResourceOptions<T> = {}) {
  const e = entry(key, fetcher as Fetcher<unknown>, opts);
  e.preparationOnly = false;
  if (!e.snap.has && !e.snap.failed && !e.promise) void load(e);
}

/**
 * A request-scoped first-page value, before the browser has read this key. It never replaces a
 * live value, error, or pending load. The first subscription revalidates in the background: the
 * event stream may have started after the server took this value.
 */
export function seedResource<T>(key: string, data: T, fetcher: Fetcher<T>, opts: ResourceOptions<T> = {}): boolean {
  if (typeof window === "undefined") return false;
  const e = entry(key, fetcher as Fetcher<unknown>, opts);
  if (e.snap.has || e.snap.failed || e.promise || e.listeners.size) return false;
  e.read = SETTLED;
  e.stale = true;
  e.settled = Date.now();
  set(e, {
    data,
    has: true,
    error: undefined,
    failed: false,
    refreshing: false,
    refreshError: undefined,
  });
  land();
  return true;
}

/**
 * `preload`, settled once its first data is in (or its first load failed). A route's loader awaits
 * it, so its page renders with its data and never shows its fallback: once a fallback is shown,
 * React reveals what replaces it no sooner than 300 ms on, however soon the data came.
 */
export function loaded<T>(key: string, fetcher: Fetcher<T>, opts: ResourceOptions<T> = {}): Promise<void> {
  const e = entry(key, fetcher as Fetcher<unknown>, opts);
  e.preparationOnly = false;
  if (e.snap.has || e.snap.failed) return Promise.resolve();
  return e.promise ?? load(e);
}

/**
 * Follows `key` outside a component, as a shown `useSuspenseResource` of it does (its first load
 * starts now): `listener` hears each change. What a store that keeps a key current uses, and tests.
 */
export function watch<T>(key: string, fetcher: Fetcher<T>, opts: ResourceOptions<T>, listener: () => void): () => void {
  const e = entry(key, fetcher as Fetcher<unknown>, opts);
  const off = e.sub(listener);
  if (!e.snap.has && !e.snap.failed && !e.promise) void load(e);
  return off;
}

/** Refreshes every key that starts with `prefix` (all of them for ""). */
export function invalidate(prefix = "") {
  for (const e of entries.values()) if (e.key.startsWith(prefix)) refresh(e);
}

/** Change the data under `key` here, at once — `useSuspenseResource`'s `setData` from outside a component. */
export function setData<T>(key: string, fn: (prev: T) => T) {
  const e = entries.get(key);
  if (e?.snap.has) set(e, { data: fn(e.snap.data as T), has: true });
}

/** `useSuspenseResource`'s `change` from outside a component: what a press shows at once, standing over a load already on its way. */
export function change<T>(key: string, fn: (prev: T) => T) {
  const e = entries.get(key);
  if (e) changed(e, fn as (prev: unknown) => unknown);
}

/** Lets the keys whose first load failed try again (`Loadable`'s Try again). */
export function retryFailed() {
  for (const e of entries.values()) if (e.snap.failed) e.snap = { ...e.snap, failed: false, error: undefined };
}

events.on(wantedTypes, (ev) => {
  if (ev.type === "resync") {
    for (const e of entries.values()) soon(e);
    return;
  }
  for (const e of entries.values()) {
    if (!e.opts.on?.includes(ev.type) || (e.opts.relevant && !e.opts.relevant(ev))) continue;
    if (e.listeners.size === 0) markStale(e);
    else if (e.opts.apply) take(e, ev);
    else soon(e);
  }
});
// The stream came back without what it missed (the server restarted, or it was away too long):
// what's shown may be out of date. One that resumed was sent what it missed.
events.onMissed(() => invalidate());
// Private first-page values and late requests belong to the login that read them.
onAuthChange((authed) => {
  if (authed) return;
  authEpoch++;
  for (const e of entries.values()) {
    e.controller?.abort();
    if (typeof window !== "undefined") {
      if (e.poll !== null) window.clearInterval(e.poll);
      window.clearTimeout(e.soon);
      window.clearTimeout(e.applying);
    }
    e.news = [];
    set(e, {
      data: undefined,
      has: false,
      error: undefined,
      failed: false,
      refreshing: false,
      refreshError: undefined,
    });
  }
  entries.clear();
});
