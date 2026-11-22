/**
 * List stability & FLIP glide helpers shared by Mission Control and Ship.
 * Kept in `lib/` so Ship never pulls `views/harness/` stylesheets (`harness.css`, `parts.css`).
 */
import * as React from "react";

/** After the pointer last moved over the list (or a finger left it), this long before it moves again. */
const QUIET_MS = 1200;
/** While one of these is open (a row's menu, a dialog from it), the list holds still (one closing doesn't count), and a windowed one unmounts no row (lib/list-window.ts). */
export const OPEN = ':is([role="menu"], [role="dialog"], [role="alertdialog"]):not([data-state="closed"])';

/**
 * Whether the list is in use, so its rows hold still: a finger or button pressed on it, focus on one
 * of its controls, a menu or dialog open, or a moment after the pointer last moved over it.
 * `bind` goes on the element around the list.
 */
export function useListHold(extra = false) {
  const [hold] = React.useState(() => {
    let pressed = false;
    let focused = false;
    let moved = 0;
    let value = false;
    let timer = 0;
    const listeners = new Set<() => void>();
    const update = () => {
      window.clearTimeout(timer);
      const quiet = Date.now() - moved;
      const open = typeof document !== "undefined" && document.querySelector(OPEN) !== null;
      const next = pressed || focused || open || quiet < QUIET_MS;
      // Released by itself: once it's quiet (and checked again while something is open).
      if (next && !pressed && !focused) timer = window.setTimeout(update, open ? 500 : QUIET_MS - quiet + 20);
      if (next !== value) {
        value = next;
        listeners.forEach((l) => l());
      }
    };
    const touch = () => {
      moved = Date.now();
      if (!value) update();
    };
    const control = (el: EventTarget | null) => el instanceof Element && el.closest("button, a, input, textarea, select, [role='menuitem']") !== null;
    return {
      subscribe: (l: () => void) => {
        listeners.add(l);
        return () => listeners.delete(l);
      },
      get: () => value,
      bind: {
        // The page scrolling (a finger flinging it on, a wheel) is the list in use as well.
        ref: (el: HTMLElement | null) => {
          if (!el) return;
          window.addEventListener("scroll", touch, { passive: true });
          return () => {
            window.removeEventListener("scroll", touch);
            window.clearTimeout(timer);
          };
        },
        onPointerDown: () => {
          pressed = true;
          moved = Date.now();
          update();
        },
        onPointerUp: () => {
          pressed = false;
          moved = Date.now();
          update();
        },
        onPointerCancel: () => {
          pressed = false;
          moved = Date.now();
          update();
        },
        onPointerMove: touch,
        onWheel: touch,
        onPointerLeave: () => {
          pressed = false;
          update();
        },
        onFocus: (e: React.FocusEvent) => {
          focused = control(e.target) && (e.target as Element).matches(":focus-visible");
          update();
        },
        onBlur: (e: React.FocusEvent) => {
          focused = e.currentTarget.contains(e.relatedTarget as Node | null) && control(e.relatedTarget);
          moved = Date.now();
          update();
        },
      },
    };
  });
  const held = React.useSyncExternalStore(hold.subscribe, hold.get, () => false);
  return { held: held || extra, bind: hold.bind };
}

/** How long a row takes to glide to its new place. */
const GLIDE_MS = 260;

/** Where each gliding item is, from the top of the list it's in. */
function places(root: HTMLElement): Map<string, number> {
  const out = new Map<string, number>();
  for (const el of root.querySelectorAll<HTMLElement>("[data-glide]")) {
    const parent = el.parentElement;
    if (parent) out.set(el.dataset.glide ?? "", el.getBoundingClientRect().top - parent.getBoundingClientRect().top);
  }
  return out;
}

/**
 * A windowed list as its glide needs it (lib/list-window.ts `ListWindow`, with the glide's `order`):
 * only the rows near the viewport are on the page, so where a row that wasn't on it was (`wasAt`,
 * from the list's first row's top) is the window's to say, and the rows near the viewport stay
 * mounted, wherever they go, until the glide that takes them there is over (`holding`, `settle`).
 */
export interface Glided {
  wasAt(el: Element): number | undefined;
  holding(): object | null;
  settle(hold: object | null): void;
}

/**
 * Items (`data-glide="<key>"`) that glide to their new places when the order (`order`) changes,
 * from where they were, rather than jump; new ones fade in. Not when motion is paused here or by the
 * device. The ref goes on the element around them. While an item glides, it can't be pressed, so a
 * tap meant for what was there doesn't land on what slid under it. In a windowed list (`win`,
 * W21-R02), a row that comes onto the page with the new order glides from where it was in the list,
 * as it would have with every row mounted, and one that leaves glides out before it's unmounted.
 */
export function useGlide(order: string, win?: Glided) {
  const [last] = React.useState(() => ({ order, root: null as HTMLElement | null, before: null as Map<string, number> | null }));
  if (last.order !== order) {
    // About to show another order: where everything is now (the page still shows the last one).
    last.order = order;
    last.before = last.root && document.documentElement.dataset.motion !== "paused" ? places(last.root) : null;
  }
  return React.useCallback(
    (el: HTMLElement | null) => {
      if (!el) return;
      last.root = el;
      const before = last.before;
      last.before = null;
      const off = () => {
        if (last.root === el) last.root = null;
      };
      // The rows the window kept for this order's glide: let go once it's over (now, when nothing glides).
      const held = win?.holding() ?? null;
      if (!before) {
        win?.settle(held);
        return off;
      }
      const runs: Animation[] = [];
      // Where the list's first row starts in it (under its top border): what the window's places are measured from.
      let origin: number | undefined;
      for (const item of el.querySelectorAll<HTMLElement>("[data-glide]")) {
        const parent = item.parentElement;
        let was = before.get(item.dataset.glide ?? "");
        if (!parent) continue;
        if (was === undefined && win && parent === el) {
          const from = win.wasAt(item);
          if (from !== undefined) {
            origin ??= (el.firstElementChild?.getBoundingClientRect().top ?? 0) - el.getBoundingClientRect().top;
            was = origin + from;
          }
        }
        if (was === undefined) {
          item.animate([{ opacity: 0 }, { opacity: 1 }], { duration: 180, easing: "ease-out" });
          continue;
        }
        const dy = was - (item.getBoundingClientRect().top - parent.getBoundingClientRect().top);
        if (Math.abs(dy) < 1) continue;
        item.style.pointerEvents = "none";
        const run = item.animate([{ transform: `translateY(${dy}px)` }, { transform: "none" }], { duration: GLIDE_MS, easing: "cubic-bezier(0.2, 0.8, 0.2, 1)" });
        const done = () => {
          item.style.pointerEvents = "";
        };
        run.onfinish = done;
        run.oncancel = done;
        runs.push(run);
      }
      if (win) void Promise.allSettled(runs.map((run) => run.finished)).then(() => win.settle(held));
      return off;
    },
    // A new ref for each order: React hands it the list once that order is on the page (and the
    // last one's cleanup runs first, so what was measured for this order is still there).
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [order],
  );
}
