/**
 * A new build of kiln went live (the server restarted onto it). This page keeps working on the
 * build it loaded: the server keeps the files of earlier builds (crate::ui). It switches to the new
 * build the next time the tab is hidden, so it never reloads under the user's hands. If a part of
 * the page can't load anyway, it reloads at once, but never twice in a row.
 */
import { events } from "@/api/events";

const RELOADED_AT = "kiln.reloaded-at";

/**
 * A build as a page's HTML names it: its import map, which names every chunk's file (a deploy that
 * changes any chunk changes it, while the entry keeps its name unless it changed itself:
 * ui/scripts/stable-names.mjs, goal 8 step 4), else its entry script.
 */
export function buildIn(html: string): string | null {
  return /<script type="importmap">([\s\S]*?)<\/script>/.exec(html)?.[1] ?? /<script[^>]+type="module"[^>]+src="([^"]+)"/.exec(html)?.[1] ?? null;
}

/** The build this page runs (production builds only; null under the dev server). */
export function thisBuild(): string | null {
  return document.querySelector('script[type="importmap"]')?.textContent ?? document.querySelector<HTMLScriptElement>('script[type="module"][src*="/assets/"]')?.getAttribute("src") ?? null;
}

/**
 * The build the server serves now. Asked for as a page: kiln answers it with the app's shell, which
 * names the build. Revalidated by its ETag, so while it's the same build it costs a 304, not the
 * shell again (goal 8: nothing downloaded twice).
 */
export async function servedBuild(): Promise<string | null> {
  const html = await (await fetch("/", { cache: "no-cache", credentials: "same-origin", headers: { accept: "text/html" } })).text();
  return buildIn(html);
}

let pending = false;

async function check() {
  if (pending) return;
  const mine = thisBuild();
  if (!mine) return;
  try {
    const now = await servedBuild();
    if (!now || now === mine) return;
  } catch {
    return; // The next reconnection checks again.
  }
  pending = true;
  if (document.hidden) reload();
  else
    document.addEventListener("visibilitychange", () => {
      if (document.hidden) reload();
    });
}

function reload() {
  try {
    sessionStorage.setItem(RELOADED_AT, String(Date.now()));
  } catch {
    /* private mode */
  }
  window.location.reload();
}

/** Reload now, unless this page reloaded in the last 10 s (then it stays as it is). */
export function reloadOnce() {
  let last = 0;
  try {
    last = Number(sessionStorage.getItem(RELOADED_AT) ?? 0);
  } catch {
    /* private mode */
  }
  if (Date.now() - last > 10_000) reload();
}

export function watchForUpdates() {
  events.onReopen(() => void check());
  // Leaving the page cancels what it was still loading, and WebKit fails those imports: no build is
  // gone, and a reload would take the page back from where it was going. Still here 5 s on (a
  // download, say), it wasn't left.
  let leaving = false;
  window.addEventListener("beforeunload", () => {
    leaving = true;
    window.setTimeout(() => (leaving = false), 5_000);
  });
  // A part of the page (a lazily loaded view) that's gone: load the new build instead of breaking.
  window.addEventListener("vite:preloadError", (e) => {
    e.preventDefault();
    if (!leaving) reloadOnce();
  });
}
