/**
 * A peered cluster opened by address (docs/peers.md): the cluster that opens it frames this app at
 * `/.kiln-peer/<slug>/` on its own address, and forwards `/.kiln-peer/<slug>/api/…` to the peer, so
 * this app, there, draws the peer. Its pages live under that base (the router's basepath), and what
 * it asks of `/api/…` goes under it too: every fetch, WebSocket, EventSource and XHR, rewritten
 * here once before anything asks (client.tsx imports this first), and the URLs pages put in links
 * and media ([`apiPath`]).
 */

/** `/.kiln-peer/<slug>` while this page draws a peered cluster, else empty. */
export const PEER_BASE = typeof location === "undefined" ? "" : (/^\/\.kiln-peer\/[^/]+/.exec(location.pathname)?.[0] ?? "");

const isApi = (path: string) => path === "/api" || path.startsWith("/api/");

/** A same-origin `/api/…` path as this page reaches it: under the base while it draws a peer. */
export function apiPath(path: string): string {
  return PEER_BASE && isApi(path) ? PEER_BASE + path : path;
}

/** `url` (anything fetch or WebSocket takes) under the base when it is this host's `/api/…`. */
function rebase(url: string): string {
  let u: URL;
  try {
    u = new URL(url, location.href);
  } catch {
    return url;
  }
  if (u.host !== location.host || !isApi(u.pathname)) return url;
  u.pathname = PEER_BASE + u.pathname;
  return u.href;
}

if (PEER_BASE) {
  const fetchHere = window.fetch.bind(window);
  window.fetch = (input: RequestInfo | URL, init?: RequestInit) => (input instanceof Request ? fetchHere(new Request(rebase(input.url), input), init) : fetchHere(rebase(String(input)), init));
  const Socket = window.WebSocket;
  window.WebSocket = class extends Socket {
    constructor(url: string | URL, protocols?: string | string[]) {
      super(rebase(String(url)), protocols);
    }
  };
  const Source = window.EventSource;
  window.EventSource = class extends Source {
    constructor(url: string | URL, init?: EventSourceInit) {
      super(rebase(String(url)), init);
    }
  };
  const open = XMLHttpRequest.prototype.open as (this: XMLHttpRequest, ...args: unknown[]) => void;
  XMLHttpRequest.prototype.open = function (this: XMLHttpRequest, method: string, url: string | URL, ...rest: unknown[]) {
    open.call(this, method, rebase(String(url)), ...rest);
  } as typeof XMLHttpRequest.prototype.open;
}
