import assert from "node:assert/strict";
import { test } from "node:test";

// Every page's first round trip: the document asks `/api/me` as it's parsed (lib/early-script.ts), and
// the app's first sign-in check takes that answer (lib/early.ts's `earlyAnswer`) where it sent a
// second request once its code was in and showed its splash until that one came back.

class Socket {
  static OPEN = 1;
  static CONNECTING = 0;
  readyState = Socket.OPEN;
  close() {}
}
const me = { authenticated: true, version: "test", hostname: "kiln" };
let requests = 0;
const fetched = async () => {
  requests++;
  return new Response(JSON.stringify({ ...me, hostname: "asked again" }));
};
const documentEvents = new EventTarget();
Object.assign(documentEvents, { fonts: { load: () => new Promise<void>(() => {}) } });
Object.assign(globalThis, {
  document: documentEvents,
  window: Object.assign(new EventTarget(), {
    setTimeout,
    clearTimeout,
    setInterval,
    clearInterval,
    fetch: fetched,
    // What the document's own script left for the app (lib/early-script.ts).
    __kilnEarly: { "/api/me": Promise.resolve(new Response(JSON.stringify(me))) },
  }),
  location: { protocol: "http:", host: "localhost", pathname: "/agents" },
  WebSocket: Socket,
  fetch: fetched,
});
await import("../src/lib/early.ts");
// lib/early.ts put its own fetch in front of the window's: the app's code calls that one.
globalThis.fetch = (globalThis as unknown as { window: { fetch: typeof fetch } }).window.fetch;
const { startAuth, checkAuth, useAuth } = await import("../src/lib/auth.ts");
const { events } = await import("../src/api/events.ts");

await test("the first sign-in check takes the document's /api/me: no request of its own; the next one asks", async () => {
  startAuth(null, "/agents");
  assert.equal(useAuth.getState().kind, "loading");
  await checkAuth();
  const first = useAuth.getState();
  assert.equal(first.kind, "authed");
  assert.equal(first.kind === "authed" && first.me?.hostname, "kiln", "the document's answer, not a second one");
  assert.equal(requests, 0, "no request before the page shows");
  await checkAuth();
  const next = useAuth.getState();
  assert.equal(requests, 1, "a later check asks the server");
  assert.equal(next.kind === "authed" && next.me?.hostname, "asked again");
  events.stop();
});
