/** W21-R02: bounded, data-only navigation intent; no hidden views, commands, or read receipts. */
import type * as React from "react";
import { isAuthed, onAuthChange } from "@/api/client";
import { prefetchSession, sessionStore, type SessionPrefetchLease } from "@/api/harness";

interface Clock {
  now(): number;
  set(fn: () => void, ms: number): ReturnType<typeof setTimeout>;
  clear(timer: ReturnType<typeof setTimeout>): void;
}
const clock: Clock = { now: Date.now, set: (fn, ms) => setTimeout(fn, ms), clear: (timer) => clearTimeout(timer) };
const BUDGET = 512 * 1024;
const DWELL_MS = 50;
const TIMEOUT_MS = 2000;
const TTL_MS = 15_000;
const KEEP = 2;
interface Intent {
  id: string;
  owners: Set<symbol>;
  at: number;
  prepare?: () => Promise<void> | undefined;
}
interface Flight extends Intent {
  lease: SessionPrefetchLease;
  timeout: ReturnType<typeof setTimeout>;
}

/** One read in flight and one latest queued intent; at most two completed speculative snapshots.
 * Thus at most 1.5 MiB of decoded snapshot input is retained/in flight (JS objects cost more).
 * Real views and pending commands are owned by SessionStore and are never evicted here.
 */
export class SessionPrefetcher {
  private queued: Intent | null = null;
  private flight: Flight | null = null;
  private dwell: ReturnType<typeof setTimeout> | null = null;
  private kept = new Map<string, { lease: SessionPrefetchLease; timer: ReturnType<typeof setTimeout> }>();

  constructor(
    private start: (id: string, budget: number) => SessionPrefetchLease | null,
    private allowed: () => boolean,
    private time: Clock = clock,
  ) {}

  intent(id: string, immediate = false, prepare?: () => Promise<void> | undefined): () => void {
    if (!id || !this.allowed() || this.kept.has(id)) return () => {};
    const owner = Symbol();
    let intent: Intent;
    if (this.flight?.id === id) intent = this.flight;
    else if (this.queued?.id === id) intent = this.queued;
    else {
      intent = { id, owners: new Set(), at: this.time.now() + (immediate ? 0 : DWELL_MS), prepare };
      this.queued = intent;
    }
    intent.prepare ??= prepare;
    intent.owners.add(owner);
    if (immediate) intent.at = this.time.now();
    this.drain();
    return () => {
      intent.owners.delete(owner);
      if (intent.owners.size) return;
      if (this.queued === intent) this.queued = null;
      if (this.flight?.owners === intent.owners) {
        this.time.clear(this.flight.timeout);
        this.flight.lease.cancel();
        this.flight = null;
      }
      this.drain();
    };
  }

  clear() {
    this.queued = null;
    if (this.dwell !== null) this.time.clear(this.dwell);
    this.dwell = null;
    if (this.flight) {
      this.time.clear(this.flight.timeout);
      this.flight.lease.cancel();
      this.flight = null;
    }
    for (const { lease, timer } of this.kept.values()) {
      this.time.clear(timer);
      lease.cancel();
    }
    this.kept.clear();
  }

  private drain() {
    if (this.dwell !== null) this.time.clear(this.dwell);
    this.dwell = null;
    if (this.flight || !this.queued) return;
    if (!this.allowed()) {
      this.queued = null;
      return;
    }
    const wait = this.queued.at - this.time.now();
    if (wait > 0) {
      this.dwell = this.time.set(() => this.drain(), wait);
      return;
    }
    const intent = this.queued;
    this.queued = null;
    // Code discovery shares the same allowed/dwell/queue policy as data. It starts alongside
    // the read, never runs route loaders or mounts a view, and failure leaves navigation its retry.
    try {
      void intent.prepare?.()?.catch(() => {});
    } catch {
      // A speculative import cannot prevent the ordinary foreground path.
    }
    const lease = this.start(intent.id, BUDGET);
    if (!lease) return;
    const flight: Flight = {
      ...intent,
      lease,
      timeout: this.time.set(() => {
        if (this.flight !== flight) return;
        this.flight = null;
        lease.cancel();
        this.drain();
      }, TIMEOUT_MS),
    };
    this.flight = flight;
    void lease.ready.then((ready) => {
      if (this.flight !== flight) return;
      this.time.clear(flight.timeout);
      this.flight = null;
      if (ready && this.allowed()) {
        const timer = this.time.set(() => this.forget(intent.id), TTL_MS);
        this.kept.set(intent.id, { lease, timer });
        while (this.kept.size > KEEP) {
          const oldest = this.kept.keys().next().value;
          if (oldest === undefined) break;
          this.forget(oldest);
        }
      } else lease.cancel();
      this.drain();
    });
  }

  private forget(id: string) {
    const kept = this.kept.get(id);
    if (!kept) return;
    this.kept.delete(id);
    this.time.clear(kept.timer);
    kept.lease.cancel();
  }
}

function allowed() {
  if (isAuthed() !== true || typeof document === "undefined" || document.hidden) return false;
  const net = typeof navigator === "undefined" ? undefined : (navigator as Navigator & { connection?: { saveData?: boolean; effectiveType?: string } });
  return net?.onLine !== false && !net?.connection?.saveData && !/^(slow-)?2g$/.test(net?.connection?.effectiveType ?? "");
}

const prefetch = new SessionPrefetcher(prefetchSession, allowed);
onAuthChange((authed) => {
  if (!authed) prefetch.clear();
});
if (typeof document !== "undefined")
  document.addEventListener("visibilitychange", () => {
    if (document.hidden) prefetch.clear();
  });

/** DOM-owned intent handlers, cleaned up on unmount. Clicking promotes before navigation unmounts
 * the source row; cancelling a hover later can never close that real view's socket.
 */
export function sessionIntent(id: string, prepare?: () => Promise<void> | undefined) {
  let hovering = false;
  let focused = false;
  let release: (() => void) | null = null;
  let touch: { x: number; y: number; timer: ReturnType<typeof setTimeout> } | null = null;
  const stop = () => {
    release?.();
    release = null;
  };
  const update = () => {
    if (hovering || focused) release ??= prefetch.intent(id, false, prepare);
    else stop();
  };
  const cancelTouch = () => {
    if (touch) clearTimeout(touch.timer);
    touch = null;
    stop();
  };
  return {
    ref: (el: HTMLElement | null) =>
      el
        ? () => {
            cancelTouch();
          }
        : undefined,
    onPointerEnter: (e: React.PointerEvent) => {
      if (e.pointerType !== "touch") {
        hovering = true;
        update();
      }
    },
    onPointerLeave: () => {
      hovering = false;
      update();
    },
    onFocus: () => {
      focused = true;
      update();
    },
    onBlur: () => {
      focused = false;
      update();
    },
    onPointerDown: (e: React.PointerEvent) => {
      if (e.pointerType !== "touch" || !e.isPrimary) return;
      cancelTouch();
      release = prefetch.intent(id, true, prepare);
      touch = { x: e.clientX, y: e.clientY, timer: setTimeout(cancelTouch, 400) };
    },
    onPointerMove: (e: React.PointerEvent) => {
      if (touch && Math.hypot(e.clientX - touch.x, e.clientY - touch.y) > 9) cancelTouch();
    },
    onPointerCancel: cancelTouch,
    onClickCapture: () => {
      void sessionStore(id).ready();
    },
  };
}
