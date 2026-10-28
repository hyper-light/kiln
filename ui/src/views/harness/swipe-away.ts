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
