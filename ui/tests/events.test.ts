import assert from "node:assert/strict";
import { test } from "node:test";

class Socket {
  static OPEN = 1;
  static all: Socket[] = [];
  readyState = 0;
  onopen?: () => void;
  onmessage?: (ev: { data: string }) => void;
  onclose?: () => void;
  onerror?: () => void;
  constructor(readonly url: string) {
    Socket.all.push(this);
  }
  send() {}
  close() {
    this.readyState = 3;
  }
  opened() {
    this.readyState = Socket.OPEN;
    this.onopen?.();
  }
  closed() {
    this.close();
    this.onclose?.();
  }
}

let timerId = 0;
const timers = new Map<number, () => void>();
const browser = Object.assign(new EventTarget(), {
  setTimeout: (fn: () => void) => {
    const id = ++timerId;
    timers.set(id, fn);
    return id;
  },
  clearTimeout: (id: number) => timers.delete(id),
});
Object.assign(globalThis, {
  WebSocket: Socket,
  location: { protocol: "http:", host: "localhost", pathname: "/" },
  document: Object.assign(new EventTarget(), { hidden: false }),
  window: browser,
});
const { events } = await import("../src/api/events.ts");

function latest() {
  const ws = Socket.all.at(-1);
  assert.ok(ws);
  return ws;
}

function retry() {
  const next = timers.entries().next().value;
  assert.ok(next, "one reconnect is scheduled");
  timers.delete(next[0]);
  next[1]();
}

await test("W21-R02 live data stays connected across subscriptions and tab visibility changes", () => {
  events.start();
  const ws = latest();
  const before = Socket.all.length;
  try {
    ws.opened();
    const off = events.subscribeState(() => {});
    off();
    events.start();
    browser.dispatchEvent(new Event("online"));
    document.dispatchEvent(new Event("visibilitychange"));
    assert.equal(Socket.all.length, before);
    assert.equal(events.getSnapshot().state, "open");
    assert.equal(timers.size, 0);
  } finally {
    events.stop();
  }
});

await test("W21-R02 a lost events connection retries once and refreshes subscribers on reopen", () => {
  let reopened = 0;
  const off = events.onReopen(() => reopened++);
  events.start();
  try {
    latest().opened();
    const before = reopened;
    latest().closed();
    assert.equal(events.getSnapshot().state, "closed");
    assert.equal(timers.size, 1);
    retry();
    assert.equal(events.getSnapshot().state, "connecting");
    latest().opened();
    assert.equal(events.getSnapshot().state, "open");
    assert.equal(reopened, before + 1);
  } finally {
    off();
    events.stop();
  }
});

await test("W21-R02 a queued open from a stopped socket cannot flicker a replacement connection to Live", () => {
  events.start();
  const stopped = latest();
  events.stop();
  events.start();
  try {
    assert.equal(events.getSnapshot().state, "connecting");
    stopped.onopen?.();
    assert.equal(events.getSnapshot().state, "connecting", "a stale socket cannot announce Live");
    latest().opened();
    assert.equal(events.getSnapshot().state, "open");
  } finally {
    events.stop();
  }
});

await test("W21-R02 a stopped socket cannot deliver events into a replacement connection", () => {
  const heard: unknown[] = [];
  const off = events.on((ev) => heard.push(ev));
  events.start();
  const stopped = latest();
  stopped.opened();
  events.stop();
  events.start();
  try {
    const current = latest();
    current.opened();
    const before = events.getSnapshot();
    stopped.onmessage?.({ data: JSON.stringify({ type: "terminals" }) });
    stopped.onmessage?.({ data: JSON.stringify({ t: "batch", at: "old.99", events: [{ type: "terminals" }] }) });
    assert.deepEqual(heard, [], "a stale socket cannot update subscribers");
    assert.equal(events.getSnapshot(), before, "a stale message cannot update connection activity");
    current.onmessage?.({ data: JSON.stringify({ type: "terminals" }) });
    assert.deepEqual(heard, [{ type: "terminals" }], "the current socket still delivers events");
    assert.notEqual(events.getSnapshot().lastMessageAt, null);
  } finally {
    off();
    events.stop();
  }
});

await test("W21-R02 a stopped socket's close cannot disconnect or retry its replacement", () => {
  events.start();
  const stopped = latest();
  stopped.opened();
  events.stop();
  events.start();
  try {
    const current = latest();
    current.opened();
    const before = events.getSnapshot();
    stopped.closed();
    assert.equal(events.getSnapshot(), before);
    assert.equal(timers.size, 0, "a stale close cannot schedule another connection");
    current.closed();
    assert.equal(events.getSnapshot().state, "closed");
    assert.equal(timers.size, 1, "only the current socket schedules a retry");
  } finally {
    events.stop();
  }
});

await test("W21-R02 stopping cancels a scheduled reconnect before a fresh start", () => {
  events.start();
  latest().opened();
  latest().closed();
  assert.equal(timers.size, 1);
  const before = Socket.all.length;
  events.stop();
  assert.equal(events.getSnapshot().state, "idle");
  assert.equal(timers.size, 0, "the pending retry was cancelled");
  browser.dispatchEvent(new Event("online"));
  document.dispatchEvent(new Event("visibilitychange"));
  assert.equal(Socket.all.length, before, "external wakeups cannot restart a stopped stream");
  events.start();
  try {
    assert.equal(Socket.all.length, before + 1, "a fresh start creates exactly one connection");
    assert.equal(timers.size, 0);
    latest().opened();
    assert.equal(events.getSnapshot().state, "open");
  } finally {
    events.stop();
  }
});
