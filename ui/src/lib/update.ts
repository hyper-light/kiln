/**
 * A new build of kiln went live (the server restarted onto it). This page keeps working on the
 * build it loaded: the server keeps the files of earlier builds (crate::ui). It switches to the new
 * build the next time the tab is hidden, so it never reloads under the user's hands. If a part of
 * the page can't load anyway, it reloads at once, but never twice in a row.
 */
import { events } from "@/api/events";

const RELOADED_AT = "kiln.reloaded-at";

/** The build this page runs: its entry script. */
function loaded(): string | null {
  return document.querySelector<HTMLScriptElement>('script[type="module"][src*="/assets/"]')?.getAttribute("src") ?? null;
}

/** The build the server serves now. */
async function served(): Promise<string | null> {
  const html = await (await fetch("/", { cache: "no-store", credentials: "same-origin" })).text();
  return /<script[^>]+type="module"[^>]+src="([^"]+)"/.exec(html)?.[1] ?? null;
}

let pending = false;

async function check() {
  if (pending) return;
  const mine = loaded();
  if (!mine) return;
  try {
    const now = await served();
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
  // A part of the page (a lazily loaded view) that's gone: load the new build instead of breaking.
  window.addEventListener("vite:preloadError", (e) => {
    e.preventDefault();
    reloadOnce();
  });
}
