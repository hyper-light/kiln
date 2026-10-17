import { useSyncExternalStore } from "react";
import { api, wsUrl } from "./client";
import type { ServerEvent } from "./types";

/**
 * One shared, reconnecting WebSocket to /api/events for the whole app. Backs off from 0.5 s to
 * 5 s (jittered), so a page is back within seconds of a server restart, and tries again at once
 * when the tab is shown or the network comes back. After a connection that was lost comes back,
 * `onReopen` listeners refresh what the page shows (resource.ts) and check for a new build
 * (update.ts).
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

class EventStream {
  private ws: WebSocket | null = null;
  private handlers = new Set<Handler>();
  private reopenListeners = new Set<() => void>();
  private stateListeners = new Set<() => void>();
  /** It was open before: the next open is a reconnection. */
  private wasOpen = false;
  private timer: number | null = null;
  private wanted = false;
  private snap: Snapshot = { state: "idle", retryAt: null, attempts: 0, lastMessageAt: null };

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

  on(fn: Handler): () => void {
    this.handlers.add(fn);
    return () => {
      this.handlers.delete(fn);
    };
  }

  /** Called whenever a lost connection is back (events may have been missed meanwhile). */
  onReopen(fn: () => void): () => void {
    this.reopenListeners.add(fn);
    return () => {
      this.reopenListeners.delete(fn);
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
    this.set({ state: "idle", retryAt: null, attempts: 0 });
  }

  /** Reconnect immediately (e.g. user clicked the indicator). */
  retryNow() {
    if (!this.wanted) return;
    if (this.timer !== null) window.clearTimeout(this.timer);
    this.timer = null;
    if (this.snap.state === "closed" || this.snap.state === "idle") this.connect();
  }

  private connect() {
    if (!this.wanted) return;
    this.set({ state: "connecting", retryAt: null });
    let opened = false;
    let ws: WebSocket;
    try {
      ws = new WebSocket(wsUrl("/api/events"));
    } catch {
      this.scheduleReconnect(false);
      return;
    }
    this.ws = ws;
    ws.onopen = () => {
      opened = true;
      this.set({ state: "open", attempts: 0 });
      if (this.wasOpen) {
        this.reopenListeners.forEach((fn) => {
          try {
            fn();
          } catch (err) {
            console.error("reopen handler failed", err);
          }
        });
      }
      this.wasOpen = true;
    };
    ws.onmessage = (msg) => {
      if (typeof msg.data !== "string") return;
      let ev: ServerEvent;
      try {
        ev = JSON.parse(msg.data) as ServerEvent;
      } catch {
        return;
      }
      this.snap = { ...this.snap, lastMessageAt: Date.now() };
      this.handlers.forEach((h) => {
        try {
          h(ev);
        } catch (err) {
          console.error("event handler failed", err);
        }
      });
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
events.on((ev) => {
  if (ev.type !== "queue") return;
  queueHistory.push({ ts: Date.now() / 1000, queued: ev.queued, running: ev.running });
  if (queueHistory.length > 2000) queueHistory.splice(0, queueHistory.length - 2000);
});

export function useConnection(): Snapshot {
  return useSyncExternalStore(events.subscribeState, events.getSnapshot, events.getSnapshot);
}

const nothing = () => 0;

/**
 * Subscribe to server events while mounted; the latest callback is always the one called (a new
 * one re-subscribes, which is cheap). No re-renders of its own: it only subscribes.
 */
export function useServerEvents(fn: Handler) {
  useSyncExternalStore((_changed) => events.on(fn), nothing, nothing);
}
