/**
 * Toasts: a word that something happened (a run finished, a save failed), at the bottom right for a
 * few seconds. `toast()` from anywhere; `<Toaster />` once, around the app. While the mouse is on
 * them, or focus is in them, they stay; × (or a swipe right, on a touch screen) dismisses one.
 */
import * as React from "react";
import { X } from "lucide-react";
import { useNavigate } from "@/lib/router";
import { StatusDot } from "./status";

export interface ToastItem {
  id: number;
  title: string;
  description?: string;
  tone?: "succeeded" | "failed" | "timed_out" | "running" | "info";
  href?: string;
  duration?: number;
}

type Shown = ToastItem & { closing?: boolean };

/** How long a toast stays, unless it says. */
const DURATION = 6000;
/** Its fade as it goes (index.css: `.toast[data-state="closed"]`). */
const FADE = 200;
/** How far a finger drags one right to dismiss it. */
const SWIPE = 64;

let seq = 0;
let items: Shown[] = [];
const listeners = new Set<() => void>();
const emit = () => listeners.forEach((l) => l());

/** Each showing toast's countdown: what it had left when it last started, and when that was. */
const timers = new Map<number, { handle: number; left: number; since: number }>();
/** While the mouse or focus is on the toasts, none counts down. */
let held = false;

export function toast(t: Omit<ToastItem, "id">) {
  const item = { ...t, id: ++seq };
  for (const old of items.slice(0, -4)) timers.delete(old.id);
  if (items.length === 0) held = false;
  items = [...items.slice(-4), item];
  const left = item.duration ?? DURATION;
  if (Number.isFinite(left)) {
    timers.set(item.id, { handle: held ? 0 : window.setTimeout(() => close(item.id), left), left, since: Date.now() });
  }
  emit();
  return item.id;
}
toast.error = (title: string, description?: string) => toast({ title, description, tone: "failed" });
toast.success = (title: string, description?: string) => toast({ title, description, tone: "succeeded" });

/** Fades it, then it's gone. */
function close(id: number) {
  const t = timers.get(id);
  if (t) window.clearTimeout(t.handle);
  timers.delete(id);
  if (!items.some((i) => i.id === id && !i.closing)) return;
  items = items.map((i) => (i.id === id ? { ...i, closing: true } : i));
  emit();
  window.setTimeout(() => {
    items = items.filter((i) => i.id !== id);
    if (items.length === 0) held = false;
    emit();
  }, FADE);
}

function hold(on: boolean) {
  if (held === on) return;
  held = on;
  const now = Date.now();
  for (const [id, t] of timers) {
    if (on) {
      window.clearTimeout(t.handle);
      t.left -= now - t.since;
    } else {
      t.since = now;
      t.handle = window.setTimeout(() => close(id), Math.max(t.left, 1500));
    }
  }
}

function subscribe(cb: () => void) {
  listeners.add(cb);
  return () => listeners.delete(cb);
}

/** A finger dragging a toast right: it follows, and past SWIPE it's dismissed. */
function swipe(id: number): Pick<React.HTMLAttributes<HTMLLIElement>, "onPointerDown" | "onPointerMove" | "onPointerUp" | "onPointerCancel"> {
  const end = (e: React.PointerEvent<HTMLLIElement>, cancel: boolean) => {
    const el = e.currentTarget;
    const dx = Number(el.dataset.dx ?? 0);
    delete el.dataset.from;
    delete el.dataset.dx;
    el.style.transition = "transform 0.2s ease";
    if (!cancel && dx > SWIPE) {
      el.style.transform = "translateX(110%)";
      close(id);
    } else {
      el.style.transform = "";
    }
  };
  return {
    onPointerDown: (e) => {
      if (e.pointerType !== "touch") return;
      e.currentTarget.dataset.from = String(e.clientX);
      e.currentTarget.style.transition = "";
    },
    onPointerMove: (e) => {
      const el = e.currentTarget;
      if (el.dataset.from === undefined) return;
      const dx = Math.max(0, e.clientX - Number(el.dataset.from));
      el.dataset.dx = String(dx);
      el.style.transform = dx > 0 ? `translateX(${dx}px)` : "";
    },
    onPointerUp: (e) => end(e, false),
    onPointerCancel: (e) => end(e, true),
  };
}

export function Toaster() {
  const list = React.useSyncExternalStore(
    subscribe,
    () => items,
    () => items,
  );
  const navigate = useNavigate();
  // Persistent requests share this corner and sit immediately above passing notifications.
  const viewport = React.useCallback((el: HTMLOListElement | null) => {
    if (!el) return;
    const root = document.documentElement;
    const measure = () => {
      const height = el.getBoundingClientRect().height;
      root.style.setProperty("--toast-stack-height", `${height > 0 ? height + 8 : 0}px`);
    };
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(el);
    return () => {
      observer.disconnect();
      root.style.removeProperty("--toast-stack-height");
    };
  }, []);
  return (
    <ol
      ref={viewport}
      className="toast-viewport"
      aria-label="Notifications"
      aria-live="polite"
      onPointerEnter={(e) => {
        if (e.pointerType !== "touch") hold(true);
      }}
      onPointerLeave={() => hold(false)}
      onFocus={() => hold(true)}
      onBlur={(e) => {
        if (!e.currentTarget.contains(e.relatedTarget as Node | null)) hold(false);
      }}
    >
      {list.map((t) => (
        <li key={t.id} className="toast touch-pan-y" data-state={t.closing ? "closed" : "open"} role={t.tone === "failed" ? "alert" : undefined} {...swipe(t.id)}>
          <span className="mt-[7px]">
            <StatusDot status={t.tone === "info" || !t.tone ? "skipped" : t.tone} />
          </span>
          <div className="min-w-0 flex-1">
            <div className="text-[13px] font-[450] leading-snug text-[#eceef2]">
              {t.href ? (
                <button
                  type="button"
                  className="text-left hover:underline hover:underline-offset-4"
                  onClick={() => {
                    navigate(t.href!);
                    close(t.id);
                  }}
                >
                  {t.title}
                </button>
              ) : (
                t.title
              )}
            </div>
            {t.description && <div className="mt-1 break-words font-mono text-[10.5px] leading-relaxed text-[#9a9da6]">{t.description}</div>}
          </div>
          <button type="button" aria-label="Dismiss" className="icon-btn -my-2 -mr-1" data-variant="bare" data-size="sm" onClick={() => close(t.id)}>
            <X size={14} />
          </button>
        </li>
      ))}
    </ol>
  );
}
