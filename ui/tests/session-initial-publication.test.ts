import assert from "node:assert/strict";
import { test } from "node:test";

class Socket {
  static OPEN = 1;
  static CONNECTING = 0;
  static CLOSED = 3;
  static all: Socket[] = [];
  readyState = Socket.OPEN;
  onmessage?: (event: { data: string }) => void;
  onclose?: () => void;
  sent: string[] = [];

  constructor(readonly url: string) {
    Socket.all.push(this);
  }

  send(value: string) {
    this.sent.push(value);
  }

  close() {
    this.readyState = Socket.CLOSED;
    this.onclose?.();
  }

  frame(value: unknown) {
    this.onmessage?.({ data: JSON.stringify(value) });
  }
}

let nextHandle = 0;
const frames = new Map<number, FrameRequestCallback>();
const timers = new Map<number, { run: () => void; delay: number }>();
Object.assign(globalThis, {
  WebSocket: Socket,
  location: { protocol: "http:", host: "localhost" },
  document: new EventTarget(),
  window: Object.assign(new EventTarget(), {
    setTimeout: (run: () => void, delay: number) => {
      const handle = ++nextHandle;
      timers.set(handle, { run, delay });
      return handle;
    },
    clearTimeout: (handle: number) => timers.delete(handle),
  }),
  requestAnimationFrame: (run: FrameRequestCallback) => {
    const handle = ++nextHandle;
    frames.set(handle, run);
    return handle;
  },
  cancelAnimationFrame: (handle: number) => frames.delete(handle),
});

const { SessionStore, sessionStore } = await import("../src/api/harness.ts");
const { setAuthed } = await import("../src/api/client.ts");
const state = { status: "running", tasks: [], delivery: {}, dropped: 0 };
const event = (seq: number) => ({ id: `e${seq}`, seq, at: seq, kind: "assistant", text: `text ${seq}` });
const incoming = (ws: Socket, id: string, events = [event(2), event(1)]) => {
  ws.frame({ t: "open", info: { id, status: "running" }, born: 77, rev: 10 });
  ws.frame({ t: "state", state });
  ws.frame({ t: "events", events });
};
const runFrame = () => {
  const queued = [...frames.values()];
  frames.clear();
  queued.forEach((run) => run(0));
};

await test("W21-R02 first ready notification includes the complete ordered snapshot without waiting for a frame", async () => {
  const store = new SessionStore("first-publication");
  const waiting = store.ready();
  const seen: ReturnType<SessionStore["get"]>[] = [];
  const off = store.subscribe(() => {
    const snap = store.get();
    seen.push(snap);
    for (const e of snap.events) assert.equal(store.event(e.id), e, "event positions publish before readiness observers");
  });
  const ws = Socket.all.at(-1)!;
  try {
    incoming(ws, store.id);
    assert.equal(store.get().ready, false);
    assert.deepEqual(store.get().events, []);
    assert.equal(store.event("e1"), undefined);
    assert.equal(seen.length, 0, "partial snapshots cannot wake the view");
    ws.frame({ t: "synced" });
    assert.equal(seen.length, 1, "the complete protocol frame notifies synchronously");
    assert.equal(frames.size, 0, "there is no obsolete first-publication frame left behind");
    assert.equal(seen[0]?.ready, true);
    assert.equal(seen[0]?.conn, "open");
    assert.equal(seen[0]?.info?.id, store.id);
    assert.equal(seen[0]?.state?.status, "running");
    assert.deepEqual(
      seen[0]?.events.map((e) => e.id),
      ["e1", "e2"],
    );
    await waiting;
    assert.equal(store.get(), seen[0]);
    runFrame();
    assert.equal(seen.length, 1, "no duplicate notification follows readiness");
  } finally {
    off();
    store.dispose();
  }
});

await test("W21-R02 ongoing stream bursts still publish once per animation frame after prompt initial readiness", () => {
  const store = new SessionStore("later-stream");
  let notifications = 0;
  const off = store.subscribe(() => notifications++);
  const ws = Socket.all.at(-1)!;
  try {
    incoming(ws, store.id, [event(1)]);
    ws.frame({ t: "synced" });
    const first = store.get().events[0];
    for (const text of [" a", " b", " c"]) ws.frame({ t: "stream", id: "e1", text });
    ws.frame({ t: "events", events: [event(3), event(2)] });
    assert.equal(notifications, 1);
    assert.equal(frames.size, 1, "the entire live burst shares one frame");
    assert.equal(store.event("e1"), first, "staged stream text is not exposed early");
    assert.equal(store.event("e2"), undefined);
    runFrame();
    assert.equal(notifications, 2);
    assert.deepEqual(
      store.get().events.map((e) => e.id),
      ["e1", "e2", "e3"],
    );
    const streamed = store.event("e1");
    assert.equal(streamed?.kind === "assistant" && streamed.text, "text 1 a b c");
    runFrame();
    assert.equal(notifications, 2);
  } finally {
    off();
    store.dispose();
  }
});

await test("W21-R02 initial publication replaces a queued error notification without dropping a later live frame", () => {
  const store = new SessionStore("queued-error");
  const seen: ReturnType<SessionStore["get"]>[] = [];
  const off = store.subscribe(() => seen.push(store.get()));
  const ws = Socket.all.at(-1)!;
  try {
    ws.frame({ t: "error", error: "temporary" });
    assert.equal(frames.size, 1);
    incoming(ws, store.id);
    ws.frame({ t: "synced" });
    assert.equal(seen.length, 1);
    assert.equal(seen[0]?.error, null);
    assert.equal(frames.size, 0);
    ws.frame({ t: "stream", id: "e1", text: " later" });
    assert.equal(frames.size, 1, "cancelling readiness's frame cannot swallow the next live update");
    runFrame();
    assert.equal(seen.length, 2);
    const streamed = store.event("e1");
    assert.equal(streamed?.kind === "assistant" && streamed.text, "text 1 later");
  } finally {
    off();
    store.dispose();
  }
});

await test("W21-R02 the first ready observer can reconnect from the completed frame's revision without another foreground read", async () => {
  const store = new SessionStore("observer-reconnect");
  let reconnected: Socket | undefined;
  let notifications = 0;
  const off = store.subscribe(() => {
    notifications++;
    if (reconnected || !store.get().ready) return;
    Socket.all.at(-1)!.readyState = Socket.CLOSED;
    store.wake();
    reconnected = Socket.all.at(-1)!;
  });
  const ws = Socket.all.at(-1)!;
  const waiting = store.ready();
  try {
    incoming(ws, store.id);
    ws.frame({ t: "synced", rev: 12 });
    assert.ok(reconnected);
    assert.notEqual(reconnected, ws);
    assert.match(reconnected.url, /[?&]after=77\.12(&|$)/, "observers see the frame's final watermark");
    await waiting;
    assert.deepEqual(
      store.get().events.map((e) => e.id),
      ["e1", "e2"],
    );
    const before = notifications;
    reconnected.frame({ t: "resumed", info: { id: store.id, status: "running" }, born: 77, rev: 14 });
    reconnected.frame({ t: "events", events: [event(3)] });
    assert.equal(store.event("e3"), undefined, "an incomplete resume stays private");
    reconnected.frame({ t: "synced" });
    assert.equal(notifications, before, "later resume notifications retain their existing frame batching");
    assert.equal(frames.size, 1);
    runFrame();
    assert.equal(notifications, before + 1);
    assert.equal(store.event("e3"), store.get().events.at(-1));
  } finally {
    off();
    store.dispose();
  }
});

await test("W21-R02 initial closed-frame readiness publishes gone and all events together", async () => {
  const store = new SessionStore("initially-closed");
  const seen: ReturnType<SessionStore["get"]>[] = [];
  const off = store.subscribe(() => seen.push(store.get()));
  const ws = Socket.all.at(-1)!;
  const waiting = store.ready();
  try {
    incoming(ws, store.id);
    ws.frame({ t: "closed" });
    assert.equal(seen.length, 1);
    assert.equal(seen[0]?.ready, true);
    assert.equal(seen[0]?.conn, "gone", "no transient open command channel leaks from commit");
    assert.deepEqual(
      seen[0]?.events.map((e) => e.id),
      ["e1", "e2"],
    );
    assert.deepEqual(ws.sent, []);
    assert.equal(frames.size, 0);
    await waiting;
  } finally {
    off();
    store.dispose();
  }
});

await test("W21-R02 promoted speculative completion notifies once and stale intent cancellation cannot dispose foreground data", async () => {
  const store = new SessionStore("initial-promoted");
  const lease = store.prefetch(4096)!;
  const ws = Socket.all.at(-1)!;
  const sockets = Socket.all.length;
  let notifications = 0;
  const off = store.subscribe(() => {
    notifications++;
    lease.cancel();
  });
  try {
    incoming(ws, store.id);
    ws.frame({ t: "synced" });
    assert.equal(notifications, 1);
    assert.equal(await lease.ready, false, "the live view owns the promoted connection");
    await store.ready();
    assert.equal(store.get().ready, true);
    assert.equal(store.event("e1"), store.get().events[0]);
    assert.equal(Socket.all.length, sockets);
    assert.equal(ws.readyState, Socket.OPEN);
    assert.deepEqual(ws.sent, [], "readiness never replays speculative commands");
    assert.equal(frames.size, 0);
  } finally {
    off();
    store.dispose();
  }
});

await test("W21-R02 auth loss inside the first ready observer cannot restore private data or a stale socket", async () => {
  setAuthed(true);
  const store = sessionStore("initial-auth-loss");
  const seen: boolean[] = [];
  const off = store.subscribe(() => {
    seen.push(store.get().ready);
    if (store.get().ready) setAuthed(false);
  });
  const ws = Socket.all.at(-1)!;
  const waiting = store.ready();
  try {
    incoming(ws, store.id);
    ws.frame({ t: "synced", rev: 12 });
    assert.deepEqual(seen, [true, false], "auth retirement publishes empty data even during an initial notification");
    assert.equal(store.get().ready, false);
    assert.deepEqual(store.get().events, []);
    assert.equal(store.event("e1"), undefined);
    assert.equal(ws.readyState, Socket.CLOSED);
    const sockets = Socket.all.length;
    ws.frame({ t: "events", events: [event(9)] });
    ws.frame({ t: "synced" });
    store.wake();
    assert.equal(store.send({ type: "interrupt" }), false);
    await waiting;
    assert.equal(Socket.all.length, sockets);
    assert.equal(store.get().ready, false);
    assert.equal(frames.size, 0);
  } finally {
    off();
    store.dispose();
    setAuthed(false);
  }
});

await test("W21-R02 legacy timer-only snapshot completion keeps its existing frame publication", () => {
  const store = new SessionStore("legacy-timer");
  let notifications = 0;
  const off = store.subscribe(() => notifications++);
  const ws = Socket.all.at(-1)!;
  try {
    incoming(ws, store.id);
    const hold = [...timers].find(([, timer]) => timer.delay === 5000);
    assert.ok(hold);
    timers.delete(hold[0]);
    hold[1].run();
    assert.equal(store.get().ready, true);
    assert.equal(notifications, 0);
    assert.equal(frames.size, 1);
    runFrame();
    assert.equal(notifications, 1);
    assert.equal(store.event("e1"), store.get().events[0]);
  } finally {
    off();
    store.dispose();
  }
});
