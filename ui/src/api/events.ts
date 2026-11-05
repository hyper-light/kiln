import { useSyncExternalStore } from "react";
import { api, wsUrl } from "./client";
import { readFrame, unread } from "./frames";
import type { ServerEvent } from "./types";
import type { EventsUp, UnreadFrame } from "./wire";

/**
 * One shared, reconnecting WebSocket to /api/events for the whole app (docs/wire.md, "The events
 * socket"). Backs off from 0.5 s to 5 s (jittered), so a page is back within seconds of a server
 * restart, and tries again at once when the tab is shown or the network comes back.
 *
 * It asks for batches (`?batch=1`): the server gathers a window's events into one frame, only the
 * newest of each thing they carry whole, none it already said. It asks only for the event types
 * something on the page listens to (`?topics=`, then a `topics` frame as they change): each
 * listener says its types (`on(types, fn)`), resource.ts the `on:` of what it holds. And it
 * resumes: a reconnect says where it was (`?after=<at>`) and is sent what it missed, so only when
 * it can't be (`onMissed`: the server restarted, or it was away too long) does what's shown load
 * again. `onReopen` listeners hear every reconnect (a new build is looked for).
 */

export type ConnState = "idle" | "connecting" | "open" | "closed";

interface Snapshot {
  state: ConnState;
  /** ms epoch when the next reconnect fires (state === "closed") */
  retryAt: number | null;
  attempts: number;
  lastMessageAt: number | null;
}

type Handler = (ev: ServerEvent) => void;

/** An event's tag. */
export type EventType = ServerEvent["type"];
/** The events tagged one of `K`. */
export type EventOf<K extends EventType> = ServerEvent & { type: K };

/** What a listener hears: some types, the types a function says as they change, or (null) all. */
type Heard = ReadonlySet<string> | (() => Iterable<string>) | null;

/** A batch, as the server writes it (`EventsDown`): its events are checked one by one. */
interface Batch {
  t: "batch";
  at: string;
  events: unknown[];
}

function isBatch(v: unknown): v is Batch {
  return typeof v === "object" && v !== null && (v as Batch).t === "batch" && typeof (v as Batch).at === "string" && Array.isArray((v as Batch).events);
}

/** The events socket's frames, by tag: a frame with another isn't applied (`frames.ts`). */
const EVENT_FRAMES = {
  resync: true,
  run: true,
  job: true,
  agent: true,
  sample: true,
  queue: true,
  terminals: true,
  updates: true,
  places: true,
  discovery: true,
  join_requests: true,
  tailscale: true,
  push: true,
  peers: true,
  keep: true,
  "machine.battery_low": true,
  ai_sessions: true,
  update_job: true,
  account_flow: true,
  plan_stack: true,
  comment: true,
  review: true,
  diff_run: true,
  attachment: true,
  incident: true,
  "preview.removed": true,
  landed: true,
  ci_image: true,
  ci_verdict: true,
  experiment: true,
  inbox: true,
  registry_package: true,
  deploy: true,
  deploy_registration: true,
  deploy_move: true,
  front_door: true,
  agent_limit: true,
  headroom: true,
  agent_change: true,
  harness: true,
  harness_closed: true,
  notices: true,
  program: true,
  cd_plan: true,
  cd_rollout: true,
  cd_app_deleted: true,
  cd_apps: true,
  "runner.offline": true,
  conflict: true,
  refused: true,
  ship: true,
  "run.succeeded": true,
  "run.fixed": true,
  "run.cancelled": true,
  "run.failed": true,
  "leg.flaky": true,
  "deploy.succeeded": true,
  "deploy.failed": true,
  "land.ejected": true,
  "diff.landed": true,
  "test.quarantined": true,
  "quota.near": true,
  "secret.unused": true,
  "approval.waiting": true,
  "review.requested": true,
  "comment.reply": true,
  mention: true,
  "change.collision": true,
  "deploy.held": true,
  "deploy.rolled_back": true,
  "deploy.rollback_failed": true,
  "agent.needs_you": true,
  "agent.finished": true,
  "agent.stopped": true,
  "agent.paused": true,
} satisfies Record<ServerEvent["type"], true>;

/** `v`, an event of a batch, read as a frame whose tag is one of the socket's. */
function readEvent(v: unknown): { frame: ServerEvent } | { unread: UnreadFrame } {
  if (typeof v !== "object" || v === null || Array.isArray(v)) return unread("", "not a JSON object");
  const t = (v as Record<string, unknown>).type;
  if (typeof t !== "string") return unread("", "no type");
  if (!Object.hasOwn(EVENT_FRAMES, t)) return unread(t, "unknown frame");
  return { frame: v as ServerEvent };
}

class EventStream {
  private ws: WebSocket | null = null;
  private handlers = new Set<Handler>();
  /** The types each of `handlers` hears. */
  private heard = new Map<Handler, Heard>();
  private reopenListeners = new Set<() => void>();
  private missedListeners = new Set<() => void>();
  private stateListeners = new Set<() => void>();
  /** It was open before: the next open is a reconnection. */
  private wasOpen = false;
  private timer: number | null = null;
  private wanted = false;
  private snap: Snapshot = { state: "idle", retryAt: null, attempts: 0, lastMessageAt: null };
  /** Where the feed was at the newest batch (`<epoch>.<seq>`): a reconnect asks for what came after. */
  private at: string | null = null;
  /** This socket's server sends batches, and so takes `topics` frames (an older one does neither). */
  private batched = false;
  /** The topics the server has for this socket, joined (null: all of them). */
  private told: string | null = null;
  /** Where the feed was when the topics last grew, until the server is told: of the types new to
   * it, the server sends what came since, so what was loaded as they were asked for misses nothing. */
  private grewAt: string | null | undefined = undefined;
  private telling = false;
  /** This open's first frame is still to come: whether it's a reconnect, and whether it asked to resume. */
  private first: { reconnect: boolean; resume: boolean } | null = null;

  getSnapshot = () => this.snap;

  subscribeState = (fn: () => void) => {
    this.stateListeners.add(fn);
    return () => {
      this.stateListeners.delete(fn);
    };
  };

  private set(patch: Partial<Snapshot>) {
    this.snap = { ...this.snap, ...patch };
    this.stateListeners.forEach((fn) => fn());
  }

  /**
   * Hear the events of `types` (only those are asked of the server while it listens; a comparison
   * with a tag not among them doesn't typecheck), of the types `types()` says as they change
   * (resource.ts: it calls `changed()`), or, with no types, every event (the server is then asked
   * for all of them).
   */
  on<K extends EventType>(types: readonly K[], fn: (ev: EventOf<K>) => void): () => void;
  on(types: () => Iterable<string>, fn: Handler): () => void;
  on(fn: Handler): () => void;
  on(a: readonly string[] | (() => Iterable<string>) | Handler, b?: (ev: never) => void): () => void {
    let heard: Heard;
    let h: Handler;
    if (Array.isArray(a)) {
      const types = new Set<string>(a);
      const fn = b as Handler | undefined;
      heard = types;
      h = (ev) => {
        if (types.has(ev.type)) fn?.(ev);
      };
    } else if (b) {
      heard = a as () => Iterable<string>;
      h = b as Handler;
    } else {
      heard = null;
      h = a as Handler;
    }
    this.handlers.add(h);
    this.heard.set(h, heard);
    this.changed();
    return () => {
      this.handlers.delete(h);
      this.heard.delete(h);
      this.changed();
    };
  }

  /** The event types the page listens to now, sorted (null: all of them). */
  topics(): string[] | null {
    const all = new Set<string>();
    for (const heard of this.heard.values()) {
      if (heard === null) return null;
      for (const t of typeof heard === "function" ? heard() : heard) all.add(t);
    }
    return [...all].sort();
  }

  /** What the page listens to may have changed: the server is told, once this task is done. */
  changed() {
    if (this.grewAt === undefined) this.grewAt = this.at;
    if (this.telling) return;
    this.telling = true;
    queueMicrotask(() => {
      this.telling = false;
      this.tell();
    });
  }

  /** Tells the server the topics, when it takes them and they're not what it has. */
  private tell() {
    const ws = this.ws;
    if (!ws || ws.readyState !== WebSocket.OPEN || !this.batched) return;
    const since = this.grewAt;
    this.grewAt = undefined;
    const now = this.topics();
    const key = now === null ? null : now.join(",");
    if (key === this.told) return;
    // `since` only when there's a type new to it: of those, what came after it is sent too.
    const had = new Set(this.told === null ? [] : this.told.split(","));
    const grew = this.told !== null && now !== null && now.some((t) => !had.has(t));
    const frame: EventsUp = now === null ? { t: "topics" } : since && grew ? { t: "topics", topics: now, since } : { t: "topics", topics: now };
    ws.send(JSON.stringify(frame));
    this.told = key;
  }

  /** Called whenever a lost connection is back (a new build may be out). */
  onReopen(fn: () => void): () => void {
    this.reopenListeners.add(fn);
    return () => {
      this.reopenListeners.delete(fn);
    };
  }

  /** Called when a connection that's back couldn't take up where it was: events were missed. */
  onMissed(fn: () => void): () => void {
    this.missedListeners.add(fn);
    return () => {
      this.missedListeners.delete(fn);
    };
  }

  start() {
    if (this.wanted) return;
    this.wanted = true;
    this.connect();
  }

  stop() {
    this.wanted = false;
    if (this.timer !== null) window.clearTimeout(this.timer);
    this.timer = null;
    const ws = this.ws;
    this.ws = null;
    ws?.close();
    this.at = null;
    this.batched = false;
    this.first = null;
    this.set({ state: "idle", retryAt: null, attempts: 0 });
  }

  /** Reconnect immediately (e.g. user clicked the indicator). */
  retryNow() {
    if (!this.wanted) return;
    if (this.timer !== null) window.clearTimeout(this.timer);
    this.timer = null;
    if (this.snap.state === "closed" || this.snap.state === "idle") this.connect();
  }

  /** Each listener hears `ev`. */
  private deliver(ev: ServerEvent) {
    this.handlers.forEach((h) => {
      try {
        h(ev);
      } catch (err) {
        console.error("event handler failed", err);
      }
    });
  }

  private fire(listeners: Set<() => void>, what: string) {
    listeners.forEach((fn) => {
      try {
        fn();
      } catch (err) {
        console.error(`${what} handler failed`, err);
      }
    });
  }

  /** This open's first frame came: a reconnect that wasn't resumed missed what came meanwhile. */
  private opened(resumed: boolean) {
    const first = this.first;
    this.first = null;
    if (first?.reconnect && !(first.resume && resumed)) this.fire(this.missedListeners, "missed");
  }

  /** A batch: where the feed is, and its events, each read as a frame of its own. */
  private batch(ws: WebSocket, data: string) {
    let v: unknown;
    try {
      v = JSON.parse(data);
    } catch (e) {
      ws.send(JSON.stringify(unread("", `not JSON: ${e instanceof Error ? e.message : String(e)}`).unread satisfies UnreadFrame));
      return;
    }
    if (!isBatch(v)) {
      ws.send(JSON.stringify(unread("batch", "not a batch").unread satisfies UnreadFrame));
      return;
    }
    this.batched = true;
    this.at = v.at;
    let events = v.events;
    if (this.first) {
      // A resume the server couldn't make starts with `resync`: what's shown loads again (`onMissed`), once.
      const resync = readEvent(events[0]);
      const missed = "frame" in resync && resync.frame.type === "resync";
      if (missed) events = events.slice(1);
      this.opened(!missed);
      // What changed while it connected.
      this.tell();
    }
    this.snap = { ...this.snap, lastMessageAt: Date.now() };
    for (const e of events) {
      const read = readEvent(e);
      if ("unread" in read) {
        if (ws.readyState === WebSocket.OPEN) ws.send(JSON.stringify(read.unread satisfies UnreadFrame));
        continue;
      }
      if (read.frame.type !== "refused") this.deliver(read.frame);
    }
  }

  private url(): string {
    const topics = this.topics();
    this.told = topics === null ? null : topics.join(",");
    this.grewAt = undefined;
    let q = "?batch=1";
    if (topics !== null) q += `&topics=${encodeURIComponent(this.told ?? "")}`;
    if (this.at) q += `&after=${encodeURIComponent(this.at)}`;
    return `/api/events${q}`;
  }

  private connect() {
    if (!this.wanted) return;
    this.set({ state: "connecting", retryAt: null });
    let opened = false;
    let ws: WebSocket;
    const resume = this.at !== null;
    this.batched = false;
    try {
      ws = new WebSocket(wsUrl(this.url()));
    } catch {
      this.scheduleReconnect(false);
      return;
    }
    this.ws = ws;
    ws.onopen = () => {
      opened = true;
      this.set({ state: "open", attempts: 0 });
      this.first = { reconnect: this.wasOpen, resume };
      if (this.wasOpen) this.fire(this.reopenListeners, "reopen");
      this.wasOpen = true;
    };
    ws.onmessage = (msg) => {
      if (typeof msg.data === "string" && msg.data.startsWith('{"t":"batch"')) {
        this.batch(ws, msg.data);
        return;
      }
      // Applied only when it reads as one of the socket's frames; else the server, which sent it, is told.
      const read = typeof msg.data === "string" ? readFrame<ServerEvent>(msg.data, "type", EVENT_FRAMES) : unread("", "a binary frame: the events socket sends text");
      if ("unread" in read) {
        if (ws.readyState === WebSocket.OPEN) ws.send(JSON.stringify(read.unread satisfies UnreadFrame));
        return;
      }
      const ev = read.frame;
      // What the server couldn't read of what this page sent: nothing to apply here.
      if (ev.type === "refused") return;
      // A frame that isn't a batch first: an older server, which doesn't resume.
      if (this.first) this.opened(false);
      this.snap = { ...this.snap, lastMessageAt: Date.now() };
      this.deliver(ev);
    };
    ws.onclose = () => {
      if (this.ws !== ws) return;
      this.ws = null;
      this.scheduleReconnect(opened);
    };
    ws.onerror = () => {
      /* onclose follows */
    };
  }

  private scheduleReconnect(wasOpen: boolean) {
    if (!this.wanted) return;
    const attempts = wasOpen ? 1 : this.snap.attempts + 1;
    const base = Math.min(5_000, 500 * 2 ** (attempts - 1));
    const delay = Math.round(base * (0.75 + Math.random() * 0.5));
    this.set({ state: "closed", attempts, retryAt: Date.now() + delay });
    // A failed upgrade might be an expired session; /api/me flips auth state on 401.
    if (!wasOpen && attempts % 2 === 0) api.me().catch(() => undefined);
    this.timer = window.setTimeout(() => {
      this.timer = null;
      this.connect();
    }, delay);
  }
}

export const events = new EventStream();

// Back at once when the tab is shown again or the network returns, not at the next backoff.
if (typeof window !== "undefined") {
  window.addEventListener("online", () => events.retryNow());
  document.addEventListener("visibilitychange", () => {
    if (!document.hidden) events.retryNow();
  });
}

/** Queue depth samples recorded from `queue` events since the console opened. */
export interface QueuePoint {
  ts: number;
  queued: number;
  running: number;
}
export const queueHistory: QueuePoint[] = [];
events.on(["queue"], (ev) => {
  // `queue` also says where a land queue's entry is: the counts are the scheduler's.
  if (!("queued" in ev)) return;
  queueHistory.push({ ts: Date.now() / 1000, queued: ev.queued, running: ev.running });
  if (queueHistory.length > 2000) queueHistory.splice(0, queueHistory.length - 2000);
});

export function useConnection(): Snapshot {
  return useSyncExternalStore(events.subscribeState, events.getSnapshot, events.getSnapshot);
}

const nothing = () => 0;

/**
 * Subscribe to server events of `types` while mounted (only those are asked of the server); the
 * latest callback is always the one called (a new one re-subscribes, which is cheap). No re-renders
 * of its own: it only subscribes.
 */
export function useServerEvents<K extends EventType>(types: readonly K[], fn: (ev: EventOf<K>) => void) {
  useSyncExternalStore((_changed) => events.on(types, fn), nothing, nothing);
}
