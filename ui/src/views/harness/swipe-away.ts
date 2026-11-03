/**
 * A finger dragging a card sideways: it follows, and past `distance` it's put away (`onAway`, once
 * it has slid out). A drag that starts on a control (a button, a link, a picker, a field) is that
 * control's, not a swipe. The approval cards (approval-toasts.tsx) and Mission control's
 * offline-machine cards (mission.tsx) put themselves away so.
 */
import type * as React from "react";

/** How far a finger drags a card sideways to put it away. */
export const SWIPE_AWAY = 64;

export function swipeAway<T extends HTMLElement>(onAway: () => void, distance = SWIPE_AWAY): Pick<React.HTMLAttributes<T>, "onPointerDown" | "onPointerMove" | "onPointerUp" | "onPointerCancel"> {
  const end = (e: React.PointerEvent<T>, cancel: boolean) => {
    const el = e.currentTarget;
    const dx = Number(el.dataset.dx ?? 0);
    delete el.dataset.from;
    delete el.dataset.dx;
    el.style.transition = "transform 0.2s ease, opacity 0.2s ease";
    if (!cancel && Math.abs(dx) > distance) {
      el.style.transform = `translateX(${dx > 0 ? 110 : -110}%)`;
      el.style.opacity = "0";
      window.setTimeout(onAway, 180);
    } else el.style.transform = "";
  };
  return {
    onPointerDown: (e) => {
      if (e.pointerType !== "touch" || (e.target as Element).closest("button, a, select, input, textarea, label")) return;
      e.currentTarget.dataset.from = String(e.clientX);
      e.currentTarget.style.transition = "";
    },
    onPointerMove: (e) => {
      const el = e.currentTarget;
      if (el.dataset.from === undefined) return;
      const dx = e.clientX - Number(el.dataset.from);
      el.dataset.dx = String(dx);
      el.style.transform = dx !== 0 ? `translateX(${dx}px)` : "";
    },
    onPointerUp: (e) => end(e, false),
    onPointerCancel: (e) => end(e, true),
  };
}

/**
 * A finger dragging a row of a list sideways (a subagent that ended, subagent-remove.tsx): as
 * `swipeAway`, but its drag may start on the row's own button (`from`, what opens it) and a tap on
 * it stays a tap: it follows only once it has gone 8 px further sideways than up or down (the list
 * scrolls as ever), and the click it ends with opens nothing. Its pick box and its buttons are theirs.
 */
export function swipeRow<T extends HTMLElement>(
  onAway: () => void,
  from: string,
  distance = SWIPE_AWAY,
): Pick<React.HTMLAttributes<T>, "onPointerDown" | "onPointerMove" | "onPointerUp" | "onPointerCancel" | "onClickCapture"> {
  const away = swipeAway<T>(onAway, distance);
  return {
    onPointerDown: (e) => {
      const t = e.target as Element;
      const control = t.closest("button, a, select, input, textarea, label");
      if (e.pointerType !== "touch" || (control && !control.matches(from))) return;
      const el = e.currentTarget;
      el.dataset.from = String(e.clientX);
      el.dataset.fromY = String(e.clientY);
      el.style.transition = "";
    },
    onPointerMove: (e) => {
      const el = e.currentTarget;
      if (el.dataset.from === undefined) return;
      const dx = e.clientX - Number(el.dataset.from);
      if (el.dataset.swiping === undefined) {
        if (Math.abs(dx) < 8 || Math.abs(dx) < Math.abs(e.clientY - Number(el.dataset.fromY))) return;
        el.dataset.swiping = "";
      }
      away.onPointerMove?.(e);
    },
    onPointerUp: (e) => {
      const el = e.currentTarget;
      const swiped = el.dataset.swiping !== undefined;
      delete el.dataset.fromY;
      delete el.dataset.swiping;
      if (!swiped) {
        delete el.dataset.from;
        return;
      }
      el.dataset.swiped = "";
      away.onPointerUp?.(e);
    },
    onPointerCancel: (e) => {
      const el = e.currentTarget;
      delete el.dataset.fromY;
      delete el.dataset.swiping;
      away.onPointerCancel?.(e);
    },
    onClickCapture: (e) => {
      const el = e.currentTarget;
      if (el.dataset.swiped === undefined) return;
      delete el.dataset.swiped;
      e.preventDefault();
      e.stopPropagation();
    },
  };
}

/**
 * A row going (a subagent removed): it slides shut, its height to nothing, then `done`, at once where
 * motion is reduced or paused, or there's no row.
 */
export function slideShut(el: HTMLElement | null, done: () => void) {
  if (!el || window.matchMedia("(prefers-reduced-motion: reduce)").matches || document.documentElement.dataset.motion === "paused") return done();
  el.style.height = `${el.getBoundingClientRect().height}px`;
  el.style.overflow = "hidden";
  el.style.transition = "height 0.2s ease, opacity 0.2s ease, padding 0.2s ease";
  void el.offsetHeight;
  el.style.height = "0px";
  el.style.opacity = "0";
  el.style.paddingTop = "0px";
  el.style.paddingBottom = "0px";
  window.setTimeout(done, 200);
}
