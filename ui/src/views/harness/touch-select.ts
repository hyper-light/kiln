/**
 * Selecting agents by touch, the way phones select things. Press and hold a row to select it (a
 * tick of the phone's vibration where it has one), and keep holding and drag over others to select
 * them too, or deselect them, as the first went. While any is selected, a tap on a row selects or
 * deselects it rather than opening it. A swipe right toggles one row without holding. A row's own
 * buttons (Allow, its menu…) work as ever. With a mouse nothing changes: rows keep their checkbox.
 *
 * Rows are found by `data-agent` under the element the returned props go on.
 */
import type * as React from "react";

/** Held this long, a press selects. */
const HOLD_MS = 420;
/** A finger that moves this far before then is scrolling (or swiping), not holding. */
const SLOP = 9;
/** Dragged this far right, a row is selected (or deselected). */
const SWIPE = 56;
/** The farthest a row follows the finger. */
const SWIPE_MAX = 88;

/** A row's own controls: they never start a selection. Its name (the button that opens it) does. */
const OWN = "a, input, select, textarea, label, [role='menuitem'], button:not(.ms-main)";

interface Press {
  id: number;
  agent: string;
  row: HTMLElement;
  x: number;
  y: number;
  timer: number;
  /** Held long enough: dragging over rows (de)selects them. */
  held: boolean;
  /** What the drag does to each row it passes: as the first row went. */
  to: boolean;
  /** Moving sideways: a swipe. */
  swiping: boolean;
  dx: number;
}

export interface TouchSelect {
  /** Whether this agent is selected now. */
  isSelected: (id: string) => boolean;
  /** Select (or deselect) these. */
  set: (ids: string[], on: boolean) => void;
  /** Whether any agent is selected: then a tap selects instead of opening. */
  selecting: boolean;
}

/** The props for the element around the rows. */
export function touchSelect(get: () => TouchSelect) {
  let press: Press | null = null;
  /** The click that follows a hold or a swipe isn't a tap. */
  let swallow = false;
  /** What pressed last: a click doesn't say in every browser (Safari's). */
  let by = "mouse";
  const rowOf = (target: EventTarget | null) => (target instanceof Element ? target.closest<HTMLElement>("[data-agent]") : null);
  const end = () => {
    if (!press) return;
    window.clearTimeout(press.timer);
    const p = press;
    press = null;
    if (p.swiping) {
      p.row.style.transition = "transform 0.18s ease";
      p.row.style.transform = "";
      window.setTimeout(() => {
        p.row.style.transition = "";
      }, 200);
    }
  };
  /** Held and dragging: the row under the finger goes as the first did. */
  const over = (x: number, y: number) => {
    const p = press;
    if (!p?.held) return;
    const under = rowOf(document.elementFromPoint(x, y))?.dataset.agent;
    const s = get();
    if (under && s.isSelected(under) !== p.to) s.set([under], p.to);
  };
  return {
    // Holding a row stops the page scrolling under the finger, so the drag selects instead. The
    // drag is followed by its touches: a browser may stop telling the pointer's moves once it
    // thinks the page should scroll.
    ref: (el: HTMLElement | null) => {
      if (!el) return;
      const move = (e: TouchEvent) => {
        if (!press?.held && !press?.swiping) return;
        e.preventDefault();
        const t = e.touches[0];
        if (t) over(t.clientX, t.clientY);
      };
      const lift = () => {
        if (press?.held) end();
      };
      el.addEventListener("touchmove", move, { passive: false });
      el.addEventListener("touchend", lift);
      el.addEventListener("touchcancel", lift);
      return () => {
        el.removeEventListener("touchmove", move);
        el.removeEventListener("touchend", lift);
        el.removeEventListener("touchcancel", lift);
      };
    },
    onPointerDown: (e: React.PointerEvent) => {
      by = e.pointerType;
      if (e.pointerType === "mouse" || !e.isPrimary) return;
      const row = rowOf(e.target);
      const agent = row?.dataset.agent;
      if (!row || !agent || (e.target as Element).closest(OWN)) return;
      end();
      swallow = false;
      const p: Press = { id: e.pointerId, agent, row, x: e.clientX, y: e.clientY, timer: 0, held: false, to: true, swiping: false, dx: 0 };
      p.timer = window.setTimeout(() => {
        if (press !== p) return;
        const s = get();
        p.held = true;
        p.to = !s.isSelected(agent);
        s.set([agent], p.to);
        swallow = true;
        navigator.vibrate?.(8);
      }, HOLD_MS);
      press = p;
    },
    onPointerMove: (e: React.PointerEvent) => {
      const p = press;
      if (!p || e.pointerId !== p.id) return;
      const dx = e.clientX - p.x;
      const dy = e.clientY - p.y;
      if (p.held) {
        over(e.clientX, e.clientY);
        return;
      }
      if (p.swiping) {
        p.dx = Math.max(0, Math.min(SWIPE_MAX, dx));
        p.row.style.transform = `translateX(${p.dx}px)`;
        p.row.dataset.swipe = p.dx > SWIPE ? "ready" : "";
        return;
      }
      if (Math.abs(dy) > SLOP) {
        // Scrolling.
        end();
        return;
      }
      if (dx > SLOP && Math.abs(dx) > Math.abs(dy) * 1.5) {
        window.clearTimeout(p.timer);
        p.swiping = true;
      }
    },
    onPointerUp: (e: React.PointerEvent) => {
      const p = press;
      if (!p || e.pointerId !== p.id) return;
      if (p.swiping) {
        swallow = true;
        delete p.row.dataset.swipe;
        if (p.dx > SWIPE) {
          const s = get();
          s.set([p.agent], !s.isSelected(p.agent));
          navigator.vibrate?.(8);
        }
      }
      end();
    },
    // Held, the drag goes on by its touches (they end it).
    onPointerCancel: () => {
      if (press?.held) return;
      if (press) delete press.row.dataset.swipe;
      end();
    },
    // A long press shows no menu of the browser's own over a row.
    onContextMenu: (e: React.MouseEvent) => {
      if (rowOf(e.target) && !(e.target as Element).closest(OWN) && (press || swallow)) e.preventDefault();
    },
    onClickCapture: (e: React.MouseEvent) => {
      const row = rowOf(e.target);
      const agent = row?.dataset.agent;
      if (!row || !agent || (e.target as Element).closest(OWN)) return;
      if (swallow) {
        swallow = false;
        e.preventDefault();
        e.stopPropagation();
        return;
      }
      // Selecting (by touch): a tap selects or deselects.
      const s = get();
      if (s.selecting && by !== "mouse") {
        e.preventDefault();
        e.stopPropagation();
        s.set([agent], !s.isSelected(agent));
      }
    },
  };
}

type Fn = (...args: unknown[]) => unknown;

/**
 * Two sets of props for one element: where both have a handler, both run (the first's first); both
 * refs are set, and both cleaned up. Make it once: its functions are new each time.
 */
export function both<A extends object, B extends object>(a: A, b: B): A & B {
  const out = { ...a, ...b } as unknown as Record<string, unknown>;
  const x = a as unknown as Record<string, unknown>;
  const y = b as unknown as Record<string, unknown>;
  for (const k of Object.keys(x)) {
    const f = x[k];
    const g = y[k];
    if (typeof f !== "function" || typeof g !== "function") continue;
    out[k] =
      k === "ref"
        ? (el: unknown) => {
            const undo = [(f as Fn)(el), (g as Fn)(el)];
            return () => undo.forEach((u) => typeof u === "function" && (u as Fn)());
          }
        : (...args: unknown[]) => {
            (f as Fn)(...args);
            (g as Fn)(...args);
          };
  }
  return out as A & B;
}
