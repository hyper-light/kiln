import assert from "node:assert/strict";
import { test } from "node:test";

class Socket {
  static OPEN = 1;
  static CONNECTING = 0;
  static CLOSED = 3;
  static all: Socket[] = [];
  readyState = Socket.OPEN;
  onmessage?: (e: { data: string }) => void;
  onclose?: () => void;
  constructor(readonly url: string) {
    Socket.all.push(this);
  }
  send() {}
  close() {
    this.readyState = Socket.CLOSED;
    this.onclose?.();
  }
  frame(e: unknown) {
    this.onmessage?.({ data: JSON.stringify(e) });
  }
}

/** The page's timers: each wait asked for and not taken back, run when the test says. */
const waits = new Map<number, { ms: number; fn: () => void }>();
let ids = 0;
/** The time, as the test sets it. */
let now = 1_000_000;
Object.assign(globalThis, {
  WebSocket: Socket,
  location: { protocol: "http:", host: "localhost" },
  document: new EventTarget(),
  window: Object.assign(new EventTarget(), {
    setTimeout: (fn: () => void, ms = 0) => {
      waits.set(++ids, { ms, fn });
      return ids;
    },
    clearTimeout: (id: number) => waits.delete(id),
  }),
  requestAnimationFrame: (fn: () => void) => setImmediate(fn),
  cancelAnimationFrame: clearImmediate,
});
Date.now = () => now;
const { SessionStore } = await import("../src/api/harness.ts");
const last = () => Socket.all.at(-1)!;
/** The session's snapshot, whole, on its newest socket. */
function snapshot() {
  const ws = last();
  ws.frame({ t: "open", info: { id: "held", status: "running" }, window: { before: 10, more: false }, rev: 10, born: 77 });
  ws.frame({ t: "history", cursor: "77.1.held", before: 10, has_more: false, oversized: false });
  ws.frame({ t: "events", events: [{ id: "e10", seq: 10, at: 10, kind: "assistant", text: "said" }] });
  ws.frame({ t: "synced" });
  return ws;
}
/** The wait that opens the next socket, run: how long it was asked to be. */
function reconnect(): number {
  const before = Socket.all.length;
  for (const [id, wait] of [...waits].reverse()) {
    waits.delete(id);
    wait.fn();
    if (Socket.all.length > before) return wait.ms;
  }
  throw new Error("no wait opened a socket");
}

test("W21-R02 a session's first socket that brings nothing and closes waits, however long it was open", () => {
  const store = new SessionStore("never");
  const off = store.subscribe(() => {});
  now += 60_000;
  last().close();
  assert.equal(reconnect(), 250);
  off();
});

test("W21-R02 a session's socket that had held is asked for again at once; one cut before its snapshot, or dropped as soon as it had it, waits", () => {
  const store = new SessionStore("held");
  const off = store.subscribe(() => {});
  const first = snapshot();
  assert.equal(store.get().ready, true);
  // It holds, then the link is lost: nothing says so but the socket closing.
  now += 60_000;
  first.close();
  assert.equal(reconnect(), 0, "asked for again at once");
  // That one closes with no snapshot (the link is still down): it waits, longer each time.
  now += 60_000;
  last().close();
  const one = reconnect();
  now += 60_000;
  last().close();
  const two = reconnect();
  assert.ok(one > 0 && two > one, `the waits grow: ${one}, ${two}`);
  // One that opens, brings nothing and closes much later was no connection either: it waits.
  now += 60_000;
  last().close();
  assert.ok(reconnect() > two, "longer again");
  // One that brings its snapshot and is dropped before it has held as long as the first wait is: a server that sends
  // a snapshot and drops the socket is asked no oftener than it was.
  const quick = snapshot();
  now += 249;
  quick.close();
  assert.equal(reconnect(), 250);
  // Held that long, at once again.
  const again = snapshot();
  now += 250;
  again.close();
  assert.equal(reconnect(), 0);
  off();
});
