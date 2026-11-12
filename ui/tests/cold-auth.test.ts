import assert from "node:assert/strict";
import { test } from "node:test";
import type { PrivateBootstrap } from "../src/lib/private-bootstrap.ts";

let fontLoaded = false;
const documentEvents = new EventTarget();
Object.assign(documentEvents, {
  fonts: { load: () => new Promise<void>(() => {}) },
});
class Socket {
  static OPEN = 1;
  static CONNECTING = 0;
  readyState = Socket.OPEN;
  close() {}
}
Object.assign(globalThis, {
  document: documentEvents,
  window: Object.assign(new EventTarget(), {
    setTimeout,
    clearTimeout,
    setInterval,
    clearInterval,
  }),
  location: { protocol: "http:", host: "localhost", pathname: "/agents/one" },
  WebSocket: Socket,
});
const { startAuth, checkAuth, useAuth, fontsReady } = await import("../src/lib/auth.ts");
const { setAuthed } = await import("../src/api/client.ts");
const { events } = await import("../src/api/events.ts");
const me = { authenticated: true, version: "test", hostname: "kiln" };
const bootstrap: PrivateBootstrap = { version: 1, pathname: "/agents/one", me };

await test("W21-R02: hydrated identity initializes once without a second auth request or font gate", async () => {
  let requests = 0,
    seeded = 0;
  globalThis.fetch = async () => {
    requests++;
    throw new Error("unexpected duplicate auth read");
  };
  void fontsReady.then(() => {
    fontLoaded = true;
  });
  startAuth(bootstrap, "/agents/one", () => {
    seeded++;
  });
  assert.equal(useAuth.getState().kind, "authed");
  assert.equal(fontLoaded, false);
  assert.equal(requests, 0);
  assert.equal(seeded, 1);
  startAuth(bootstrap, "/agents/one", () => {
    seeded++;
  });
  assert.equal(seeded, 1, "StrictMode replay does not seed or authenticate again");
  setAuthed(false);
  startAuth(bootstrap, "/agents/one", () => {
    seeded++;
  });
  assert.equal(useAuth.getState().kind, "anon");
  assert.equal(seeded, 1, "the old document cannot authenticate another login");
});

await test("W21-R02: late auth response after logout cannot revive identity; a new check remains independent", async () => {
  let resolveOld = (_value: Response) => {};
  globalThis.fetch = () =>
    new Promise<Response>((resolve) => {
      resolveOld = resolve;
    });
  setAuthed(true);
  const old = checkAuth();
  setAuthed(false);
  globalThis.fetch = async () => new Response(JSON.stringify({ ...me, authenticated: false }));
  const next = checkAuth();
  resolveOld(new Response(JSON.stringify(me)));
  await Promise.all([old, next]);
  assert.equal(useAuth.getState().kind, "anon");
  events.stop();
});

await test("W21-R02: a previous login's late 401 cannot sign out a newly verified identity", async () => {
  let resolveOld = (_value: Response) => {};
  globalThis.fetch = () =>
    new Promise<Response>((resolve) => {
      resolveOld = resolve;
    });
  const old = checkAuth();
  setAuthed(true);
  setAuthed(false);
  globalThis.fetch = async () => new Response(JSON.stringify(me));
  await checkAuth();
  assert.equal(useAuth.getState().kind, "authed");
  resolveOld(new Response("old session expired", { status: 401 }));
  await old;
  assert.equal(useAuth.getState().kind, "authed");
  events.stop();
});
