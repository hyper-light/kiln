/** Shorter than this, the window is a phone's with its keyboard (or a picker) up. */
const SHORT_PX = 560;

/**
 * W21-R13: fit full-height pages during their mounting commit, before the browser can paint an
 * unfitted shell. Enter the existing overflow mode before measuring, as the original controller
 * did. Viewport/keyboard events remain immediate; read geometry before height/pan writes and
 * write only changed values. Correctness does not depend on a deferred first fit.
 */
export function fitTheWindow(el: HTMLElement | null) {
  if (!el) return;
  const root = document.documentElement;
  const win = window;
  const view = win.visualViewport;
  let closed = false;
  let fitted = false;
  let height: number | undefined;
  let short: boolean | undefined;

  const fit = () => {
    if (closed) return;
    if (!fitted) {
      root.classList.add("fills-window");
      fitted = true;
    }
    // Zoom changes CSS pixels too. Keep the height in unzoomed pixels even as the keyboard moves.
    const scale = view?.scale ?? 1;
    const h = Math.round(view ? view.height * scale : win.innerHeight);
    const isShort = h < SHORT_PX;
    const moved = win.scrollY !== 0 || win.scrollX !== 0 || (view?.offsetTop ?? 0) > 0;
    const panBack = Math.abs(scale - 1) <= 0.01 && moved;

    if (height !== h) {
      root.style.setProperty("--window-h", `${h}px`);
      height = h;
    }
    if (short !== isShort) {
      if (isShort) root.dataset.window = "short";
      else delete root.dataset.window;
      short = isShort;
    }
    // A same-height viewport event can still be iOS panning above its keyboard. Leave zoom alone.
    if (panBack) win.scrollTo({ top: 0, left: 0, behavior: "instant" });
  };

  view?.addEventListener("resize", fit);
  view?.addEventListener("scroll", fit);
  win.addEventListener("resize", fit);
  win.addEventListener("pageshow", fit);
  win.addEventListener("scroll", fit, { passive: true });
  fit();

  return () => {
    if (closed) return;
    closed = true;
    view?.removeEventListener("resize", fit);
    view?.removeEventListener("scroll", fit);
    win.removeEventListener("resize", fit);
    win.removeEventListener("pageshow", fit);
    win.removeEventListener("scroll", fit);
    if (fitted) {
      root.classList.remove("fills-window");
      root.style.removeProperty("--window-h");
      delete root.dataset.window;
    }
  };
}
