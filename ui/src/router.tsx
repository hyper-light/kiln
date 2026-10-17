/**
 * kiln's router (TanStack Router, under TanStack Start). Pages move into it from the app it still
 * hosts (`routes/$.tsx`) one at a time.
 */
import { createRouter } from "@tanstack/react-router";
import { routeTree } from "./routeTree.gen";

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

export function getRouter() {
  return createRouter({ routeTree, scrollRestoration: true, parseSearch, stringifySearch });
}

declare module "@tanstack/react-router" {
  interface Register {
    router: ReturnType<typeof getRouter>;
  }
}
