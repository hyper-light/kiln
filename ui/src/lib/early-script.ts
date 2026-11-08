/**
 * What a page reads as it opens, asked by the document itself as it's parsed (routes/__root.tsx's
 * head), before its code is in: lib/early.ts hands those answers to the app's own reads.
 */

import { SIGNED_OUT } from "@/api/client";
import { SHIP_FIRST_READ } from "@/api/ship-first";

/** What every page reads first. */
const SHELL = ["/api/me", "/api/peers/open"];

/**
 * What a page reads as it opens, by its path (after a peer's base): what its first render waits on,
 * taken by the app's own reads. What it reads after (Mission control's changes, its AI sessions)
 * would only take the connections its code needs.
 */
export const EARLY: Record<string, readonly string[]> = {
  "/agents": [
    "/api/harness/sessions",
    "/api/harness/kinds",
    "/api/agents",
    "/api/terminal/sessions",
    "/api/reach/runners",
    "/api/harness/workspaces",
    "/api/push",
    "/api/push/subscriptions",
    "/api/voice",
  ],
  // Ship's rows, each repository's newest commit: what its list waits on (a facet chosen in the
  // address reads another URL, and the app reads that itself).
  "/ship": [SHIP_FIRST_READ],
};

/** The document's own script: asks the reads of its path, under a peer's base (lib/peer-base.ts), and keeps them for lib/early.ts's fetch: only who this is when it was signed out last time. Only the server has it: it's in the document, not the code. */
export const EARLY_SCRIPT =
  `(function(){var b=(/^\\/\\.kiln-peer\\/[^/]+/.exec(location.pathname)||[""])[0],p=location.pathname.slice(b.length).replace(/\\/+$/,"")||"/",e=window.__kilnEarly={};` +
  `var o;try{o=localStorage.getItem(${JSON.stringify(SIGNED_OUT)})==="1"}catch(x){}` +
  `${JSON.stringify(SHELL)}.concat((!o&&${JSON.stringify(EARLY)}[p])||[]).forEach(function(u){(e[u]=fetch(b+u,{credentials:"same-origin"})).catch(function(){})})})();`;
