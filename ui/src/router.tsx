/**
 * kiln's router (TanStack Router, under TanStack Start). Pages move into it from the app it still
 * hosts (`routes/$.tsx`) one at a time.
 */
import { createRouter, type LocationRewrite } from "@tanstack/react-router";
import { routeTree } from "./routeTree.gen";
import { PEER_BASE } from "./lib/peer-base";

/**
 * Search params as plain strings, as the URL has them (TanStack's default reads JSON): what the
 * pages expect (lib/router.tsx's useSearchParams).
 */
function parseSearch(s: string): Record<string, string> {
  return Object.fromEntries(new URLSearchParams(s));
}

function stringifySearch(o: Record<string, unknown>): string {
  const q = new URLSearchParams();
  for (const [k, v] of Object.entries(o)) if (v !== undefined && v !== null) q.set(k, String(v));
  const s = q.toString();
  return s ? `?${s}` : "";
}

/**
 * An agent's conversation keeps your place itself (views/harness/timeline.tsx), so the router
 * doesn't scroll there: not back to where you were in pixels, nor where the page before was. A
 * conversation that grew since, or is another agent's, puts those pixels somewhere else (often far
 * back), and after the page has put you where you were.
 */
const keepsItsPlace = (path: string) => /^\/agents\/(?!plans(?:\/|$)|sessions(?:\/|$))[^/]+\/?$/.test(path);

/**
 * Drawing a peered cluster opened by address, every page is under its path (lib/peer-base.ts): the
 * path comes off the address as the router reads it, and goes back on the addresses it makes. A
 * rewrite rather than `basepath`, which TanStack Start sets again as it hydrates.
 */
const underPeer: LocationRewrite | undefined = PEER_BASE
  ? {
      input: ({ url }) => {
        if (url.pathname === PEER_BASE) url.pathname = "/";
        else if (url.pathname.startsWith(`${PEER_BASE}/`)) url.pathname = url.pathname.slice(PEER_BASE.length);
        return url;
      },
      output: ({ url }) => {
        url.pathname = `${PEER_BASE}${url.pathname}`;
        return url;
      },
    }
  : undefined;

export function getRouter() {
  return createRouter({ routeTree, rewrite: underPeer, scrollRestoration: ({ location }) => !keepsItsPlace(location.pathname), parseSearch, stringifySearch });
}

declare module "@tanstack/react-router" {
  interface Register {
    router: ReturnType<typeof getRouter>;
  }
}
