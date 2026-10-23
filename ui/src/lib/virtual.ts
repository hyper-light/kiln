/**
 * A windowed list on the page's own scroll: only the rows near the viewport are rendered, the
 * rest are spacers of the right height, so a list of 100,000 rows costs what forty do.
 * Rows have one fixed height; the list re-measures on scroll and resize, once a frame.
 */
import * as React from "react";

export interface Window {
  /** First row rendered. */
  start: number;
  /** One past the last row rendered. */
  end: number;
}

/**
 * The rows to render, and the ref for the list's element. The ref measures as it attaches (before
 * paint, so a list never shows a frame without its rows) and again whenever the count changes.
 * `onEnd`: called when the rows shown come within `ahead` rows of the end (load the next page).
 */
export function useWindowedRows(
  count: number,
  rowHeight: number,
  { overscan = 10, ahead = 50, onEnd }: { overscan?: number; ahead?: number; onEnd?: () => void } = {},
): [ref: (el: HTMLElement | null) => (() => void) | undefined, win: Window] {
  const [win, setWin] = React.useState<Window>({ start: 0, end: Math.min(count, 40) });
  const ref = React.useCallback(
    (el: HTMLElement | null) => {
      if (!el) return;
      let frame = 0;
      const measure = () => {
        frame = 0;
        const top = -el.getBoundingClientRect().top;
        const start = Math.max(0, Math.floor(top / rowHeight) - overscan);
        const end = Math.min(count, Math.ceil((top + window.innerHeight) / rowHeight) + overscan);
        setWin((w) => (w.start === start && w.end === end ? w : { start, end }));
        if (end >= count - ahead) onEnd?.();
      };
      const schedule = () => {
        if (!frame) frame = requestAnimationFrame(measure);
      };
      measure();
      window.addEventListener("scroll", schedule, { passive: true });
      window.addEventListener("resize", schedule);
      return () => {
        window.removeEventListener("scroll", schedule);
        window.removeEventListener("resize", schedule);
        if (frame) cancelAnimationFrame(frame);
      };
    },
    [count, rowHeight, overscan, ahead, onEnd],
  );
  return [ref, { start: Math.min(win.start, count), end: Math.min(win.end, count) }];
}

/**
 * A windowed list in a box of its own that scrolls (a side list): only the rows near what the box
 * shows are rendered, `[data-row]` the rows, their height measured once from the first one drawn
 * (`guess` until then). The ref measures as it attaches and on the box's scroll, once a frame.
 */
export function useWindowedBox(count: number, guess: number, { overscan = 10 }: { overscan?: number } = {}): [ref: (el: HTMLElement | null) => (() => void) | undefined, win: Window, row: number] {
  const [state, setState] = React.useState({ start: 0, end: Math.min(count, 40), row: guess });
  const ref = React.useCallback(
    (el: HTMLElement | null) => {
      if (!el) return;
      let frame = 0;
      let row = 0;
      const measure = () => {
        frame = 0;
        if (!row) row = el.querySelector<HTMLElement>("[data-row]")?.getBoundingClientRect().height ?? 0;
        const h = row || guess;
        const start = Math.max(0, Math.floor(el.scrollTop / h) - overscan);
        const end = Math.min(count, Math.ceil((el.scrollTop + el.clientHeight) / h) + overscan);
        setState((s) => (s.start === start && s.end === end && s.row === h ? s : { start, end, row: h }));
      };
      const schedule = () => {
        if (!frame) frame = requestAnimationFrame(measure);
      };
      measure();
      el.addEventListener("scroll", schedule, { passive: true });
      window.addEventListener("resize", schedule);
      return () => {
        el.removeEventListener("scroll", schedule);
        window.removeEventListener("resize", schedule);
        if (frame) cancelAnimationFrame(frame);
      };
    },
    [count, guess, overscan],
  );
  return [ref, { start: Math.min(state.start, count), end: Math.min(state.end, count) }, state.row];
}
