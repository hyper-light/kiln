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
 */
export function useWindowedRows(count: number, rowHeight: number, overscan = 10): [ref: (el: HTMLElement | null) => (() => void) | undefined, win: Window] {
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
    [count, rowHeight, overscan],
  );
  return [ref, { start: Math.min(win.start, count), end: Math.min(win.end, count) }];
}
