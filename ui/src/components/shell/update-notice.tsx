/**
 * "kiln was updated": a page left open across a deploy keeps running the build it loaded (it
 * switches by itself the next time the tab is hidden, lib/update.ts). This says so in the header,
 * with a Reload for now. It compares the build this page runs with the one the server serves: when
 * the event stream reconnects (servers restart on deploy), when the tab comes back into view, and
 * every few minutes. Checked outside React; the header only reads the answer.
 */
import { useSyncExternalStore } from "react";
import { RefreshCw } from "lucide-react";
import { events } from "@/api/events";
import { servedBuild, thisBuild } from "@/lib/update";

const EVERY_MS = 5 * 60_000;

let stale = false;
let started = false;
const listeners = new Set<() => void>();

async function check() {
  const mine = thisBuild();
  if (!mine || stale) return;
  try {
    const served = await servedBuild();
    if (served && served !== mine) {
      stale = true;
      listeners.forEach((l) => l());
    }
  } catch {
    /* offline: the next reconnection asks again */
  }
}

function start() {
  if (started) return;
  started = true;
  events.onReopen(() => void check());
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "visible") void check();
  });
  window.setInterval(() => void check(), EVERY_MS);
  // Not as the page starts: it has just loaded the build the server serves, and on a slow link the
  // shell asked again then is bytes the page's own code waits behind (goal 8 step 6).
}

function subscribe(listener: () => void) {
  start();
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

/** In the header, beside the connection: only once a newer build is live. */
export function UpdateNotice() {
  const updated = useSyncExternalStore(
    subscribe,
    () => stale,
    () => false,
  );
  if (!updated) return null;
  return (
    <span className="update-notice" role="status">
      <span className="max-sm:sr-only">kiln was updated</span>
      <button type="button" onClick={() => window.location.reload()}>
        <RefreshCw size={13} aria-hidden="true" /> Reload
      </button>
    </span>
  );
}
