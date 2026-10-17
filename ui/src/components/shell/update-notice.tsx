/**
 * "kiln was updated": a page left open across a deploy keeps running the old app until it is
 * reloaded, and nothing would say so. This compares the app this page runs with the one the server
 * now serves — when the event stream reconnects (servers restart on deploy), when the tab comes
 * back into view, and every few minutes — and offers a reload when they differ.
 */
import * as React from "react";
import { RefreshCw } from "lucide-react";
import { useConnection } from "@/api/events";

const EVERY_MS = 5 * 60_000;

/** The entry script this page loaded (production builds only; null under the dev server). */
function loadedApp(): string | null {
  const s = document.querySelector<HTMLScriptElement>('script[type="module"][src*="/assets/index-"]');
  const m = s?.getAttribute("src")?.match(/\/assets\/index-[^"'?#]+\.js/);
  return m ? m[0] : null;
}

export function UpdateNotice() {
  const conn = useConnection();
  const [stale, setStale] = React.useState(false);
  const mine = React.useMemo(loadedApp, []);
  const check = React.useCallback(async () => {
    if (!mine) return;
    try {
      const html = await (await fetch("/", { cache: "no-store", credentials: "same-origin" })).text();
      const served = html.match(/\/assets\/index-[^"'?#]+\.js/)?.[0];
      if (served && served !== mine) setStale(true);
    } catch {
      /* offline: try again later */
    }
  }, [mine]);

  const wasClosed = React.useRef(false);
  React.useEffect(() => {
    if (conn.state === "closed") wasClosed.current = true;
    if (conn.state === "open" && wasClosed.current) {
      wasClosed.current = false;
      void check();
    }
  }, [conn.state, check]);

  React.useEffect(() => {
    const onVisible = () => document.visibilityState === "visible" && void check();
    document.addEventListener("visibilitychange", onVisible);
    const t = window.setInterval(() => void check(), EVERY_MS);
    void check();
    return () => {
      document.removeEventListener("visibilitychange", onVisible);
      window.clearInterval(t);
    };
  }, [check]);

  if (!stale) return null;
  return (
    <div className="update-notice" role="status">
      <span>kiln was updated</span>
      <button type="button" onClick={() => window.location.reload()}>
        <RefreshCw size={13} /> Reload
      </button>
    </div>
  );
}
