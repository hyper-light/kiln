import assert from "node:assert/strict";
import { test } from "node:test";

/** A browser's socket, as the test opens and closes it. */
class Socket {
  static OPEN = 1;
  static CONNECTING = 0;
  static CLOSED = 3;
  static all: Socket[] = [];
  readyState = Socket.CONNECTING;
  url: string;
  onopen?: () => void;
  onmessage?: (ev: { data: string }) => void;
  onclose?: () => void;
  onerror?: () => void;
  constructor(url?: string) {
    this.url = url ?? "";
    Socket.all.push(this);
  }
  send() {}
  opens() {
    this.readyState = Socket.OPEN;
    this.onopen?.();
  }
  close() {
    this.readyState = Socket.CLOSED;
    this.onclose?.();
  }
}

/** The page's timers: each wait asked for, and what it's for, run when the test says. */
const waits: { ms: number; fn: () => void }[] = [];
/** The time, as the test sets it. */
let now = 1_000_000;
Object.assign(globalThis, {
  WebSocket: Socket,
  location: { protocol: "http:", host: "localhost" },
  document: Object.assign(new EventTarget(), { hidden: false }),
  window: Object.assign(new EventTarget(), {
    setTimeout: (fn: () => void, ms = 0) => waits.push({ ms, fn }),
    clearTimeout: () => {},
  }),
  fetch: () => Promise.reject(new Error("no server")),
});
Date.now = () => now;
const { events } = await import("../src/api/events.ts");
const last = () => Socket.all.at(-1)!;
/** The wait asked for since the last one run, run now. */
const run = () => {
  const wait = waits.pop();
  waits.length = 0;
  wait?.fn();
  return wait?.ms;
};

test("W21-R02 a socket that had held is asked for again at once; one that never opened, or opened and dropped, waits", () => {
  events.start();
  const first = last();
  first.opens();
  // It holds, then the link is lost: nothing says so but the socket closing.
  now += 60_000;
  first.close();
  assert.equal(events.getSnapshot().state, "closed");
  assert.equal(run(), 0, "asked for again at once");
  const second = last();
  assert.notEqual(second, first);
  assert.equal(events.getSnapshot().state, "connecting");

  // That one never opens (the link is still down): it waits, longer each time.
  second.close();
  const waited = [run()!];
  last().close();
  waited.push(run()!);
  last().close();
  waited.push(run()!);
  assert.ok(
    waited.every((ms, i) => ms > 0 && (i === 0 || ms > waited[i - 1]! / 2)),
    `${waited}`,
  );
  assert.ok(waited[0]! >= 750 && waited[2]! > waited[0]!, `the waits grow: ${waited}`);

  // One that opens and is dropped before it has held as long as the first wait is: a server taking sockets and
  // dropping them is asked no oftener than it was.
  last().opens();
  now += 499;
  last().close();
  const dropped = run()!;
  assert.ok(dropped >= 375 && dropped <= 625, `${dropped}`);
  // Held that long, at once again.
  last().opens();
  now += 500;
  last().close();
  assert.equal(run(), 0);
  events.stop();
});
