/**
 * Tooltips: a word about a control, shown while a mouse rests on it or the keyboard is on it (never
 * on a tap). Moving from one control to the next shows the next at once. Placed beside the control
 * as it opens and kept there as the page scrolls, by this file alone: every page has tooltips, so
 * they carry no positioning library.
 */
import * as React from "react";
import { createPortal } from "react-dom";

type Side = "top" | "bottom" | "left" | "right";

/** How long a mouse rests on a control before its tooltip shows. */
const DELAY = 350;
/** Within this long of one closing, the next shows at once. */
const SKIP = 300;
/** Kept clear of the window's edges. */
const EDGE = 12;
/** Between the control and its tooltip. */
const GAP = 8;

const OPPOSITE: Record<Side, Side> = { top: "bottom", bottom: "top", left: "right", right: "left" };

let closedAt = 0;

/** What a control wrapped in a Tip is given (it passes them to its element, as any trigger must). */
interface TriggerProps {
  "aria-describedby"?: string;
  onPointerEnter?: React.PointerEventHandler<HTMLElement>;
  onPointerLeave?: React.PointerEventHandler<HTMLElement>;
  onPointerDown?: React.PointerEventHandler<HTMLElement>;
  onFocus?: React.FocusEventHandler<HTMLElement>;
  onBlur?: React.FocusEventHandler<HTMLElement>;
  onKeyDown?: React.KeyboardEventHandler<HTMLElement>;
}

/** Wrap a single focusable child with a tooltip. */
export function Tip({ content, children, side = "top" }: { content: React.ReactNode; children: React.ReactElement<TriggerProps>; side?: Side }) {
  const id = React.useId();
  const [at, setAt] = React.useState<HTMLElement | null>(null);
  const timer = React.useRef(0);
  const open = React.useRef(false);

  const show = (el: HTMLElement, now: boolean) => {
    window.clearTimeout(timer.current);
    timer.current = window.setTimeout(
      () => {
        open.current = true;
        setAt(el);
      },
      now || Date.now() - closedAt < SKIP ? 0 : DELAY,
    );
  };
  const hide = () => {
    window.clearTimeout(timer.current);
    if (open.current) closedAt = Date.now();
    open.current = false;
    setAt(null);
  };

  const p = children.props;
  const trigger = React.cloneElement(children, {
    "aria-describedby": at ? [p["aria-describedby"], id].filter(Boolean).join(" ") : p["aria-describedby"],
    onPointerEnter: (e) => {
      p.onPointerEnter?.(e);
      if (e.pointerType !== "touch") show(e.currentTarget, false);
    },
    onPointerLeave: (e) => {
      p.onPointerLeave?.(e);
      hide();
    },
    onPointerDown: (e) => {
      p.onPointerDown?.(e);
      hide();
    },
    // From the keyboard only: a click focuses the control too, and that's not asking.
    onFocus: (e) => {
      p.onFocus?.(e);
      if (e.currentTarget.matches(":focus-visible")) show(e.currentTarget, true);
    },
    onBlur: (e) => {
      p.onBlur?.(e);
      hide();
    },
    onKeyDown: (e) => {
      p.onKeyDown?.(e);
      if (e.key === "Escape" && open.current) {
        e.stopPropagation(); // this Escape closes the tooltip, not the dialog it's in
        hide();
      }
    },
  });

  return (
    <>
      {trigger}
      {at &&
        createPortal(
          <div
            ref={(el) => (el ? follow(el, at, side) : undefined)}
            role="tooltip"
            id={id}
            className="pointer-events-none fixed left-0 top-0 z-[100] max-w-[min(20rem,calc(100vw-2rem))] rounded-lg border border-white/15 bg-[#191a1d] px-3 py-2 text-xs leading-relaxed text-[#e4e5e7] shadow-xl"
          >
            {content}
          </div>,
          document.body,
        )}
    </>
  );
}

/**
 * Places the tooltip beside its control (on the other side when there's no room), inside the
 * window, before it's painted, and again as anything scrolls or the window resizes, until it closes.
 */
function follow(tip: HTMLElement, anchor: HTMLElement, side: Side): () => void {
  const place = () => {
    const a = anchor.getBoundingClientRect();
    const w = tip.offsetWidth;
    const h = tip.offsetHeight;
    const vw = document.documentElement.clientWidth;
    const vh = document.documentElement.clientHeight;
    const room: Record<Side, boolean> = {
      top: a.top - GAP - h >= EDGE,
      bottom: a.bottom + GAP + h <= vh - EDGE,
      left: a.left - GAP - w >= EDGE,
      right: a.right + GAP + w <= vw - EDGE,
    };
    const s = room[side] || !room[OPPOSITE[side]] ? side : OPPOSITE[side];
    const x = s === "left" ? a.left - GAP - w : s === "right" ? a.right + GAP : a.left + a.width / 2 - w / 2;
    const y = s === "top" ? a.top - GAP - h : s === "bottom" ? a.bottom + GAP : a.top + a.height / 2 - h / 2;
    const cx = Math.min(Math.max(x, EDGE), vw - EDGE - w);
    const cy = Math.min(Math.max(y, EDGE), vh - EDGE - h);
    tip.style.transform = `translate(${Math.round(cx)}px, ${Math.round(cy)}px)`;
    tip.dataset.side = s;
  };
  place();
  window.addEventListener("scroll", place, { capture: true, passive: true });
  window.addEventListener("resize", place, { passive: true });
  return () => {
    window.removeEventListener("scroll", place, { capture: true });
    window.removeEventListener("resize", place);
  };
}
