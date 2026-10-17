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

export function useWindowedRows(ref: React.RefObject<HTMLElement | null>, count: number, rowHeight: number, overscan = 10): Window {
  const [win, setWin] = React.useState<Window>({ start: 0, end: Math.min(count, 40) });
  // Measured before paint, so a list never shows a frame without its rows.
  React.useLayoutEffect(() => {
    let frame = 0;
    const measure = () => {
      frame = 0;
      const el = ref.current;
      if (!el) return;
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
  }, [ref, count, rowHeight, overscan]);
  return { start: Math.min(win.start, count), end: Math.min(win.end, count) };
}
