import assert from "node:assert/strict";
import { test } from "node:test";

class Socket {
  static CONNECTING = 0;
  static OPEN = 1;
  static CLOSING = 2;
  static CLOSED = 3;
  static made: Socket[] = [];
  readyState = Socket.CONNECTING;
  binaryType = "blob";
  onmessage: ((m: MessageEvent) => void) | null = null;
  onclose: (() => void) | null = null;
  constructor(readonly url: string) {
    Socket.made.push(this);
  }
  close() {
    this.readyState = Socket.CLOSED;
    this.onclose?.();
  }
}

Object.assign(globalThis, {
  WebSocket: Socket,
  location: { protocol: "http:", host: "localhost", pathname: "/agents/a", href: "http://localhost/agents/a" },
});

const { sessionSocketUrl, startSession, storesHere, takeSessionSocket } = await import("../src/api/session-socket.ts");

await test("W21-R02 goal 8: a conversation's socket opened ahead is taken with what it heard, in order, by the store asking for it", () => {
  const leaving = new AbortController();
  startSession("a", leaving.signal);
  startSession("a", leaving.signal);
  assert.equal(Socket.made.length, 1, "one socket a session");
  const ws = Socket.made[0]!;
  assert.equal(ws.url, sessionSocketUrl("a", true));
  assert.equal(ws.binaryType, "arraybuffer");
  ws.readyState = Socket.OPEN;
  ws.onmessage?.({ data: "one" } as MessageEvent);
  ws.onmessage?.({ data: "two" } as MessageEvent);
  const taken = takeSessionSocket("a", sessionSocketUrl("a", true));
  assert.equal(taken?.ws, ws as unknown as WebSocket);
  assert.deepEqual(
    taken?.heard.map((m) => m.data),
    ["one", "two"],
  );
  assert.equal(takeSessionSocket("a", sessionSocketUrl("a", true)), undefined, "taken once");
  leaving.abort();
  assert.equal(ws.readyState, Socket.OPEN, "the store's now: leaving the opening doesn't close it");
});

await test("W21-R02 goal 8: a socket opened ahead closes when the opening is left, or its store asks for another", () => {
  const left = new AbortController();
  startSession("b", left.signal);
  const b = Socket.made.at(-1)!;
  left.abort();
  assert.equal(b.readyState, Socket.CLOSED);
  assert.equal(takeSessionSocket("b", sessionSocketUrl("b", true)), undefined);

  startSession("c", new AbortController().signal);
  const c = Socket.made.at(-1)!;
  assert.equal(takeSessionSocket("c", sessionSocketUrl("c", false, "&after=1.2")), undefined);
  assert.equal(c.readyState, Socket.CLOSED, "what it asked for isn't what the store asks for");
});

await test("W21-R02 goal 8: once the session store is here, opening a page readies its store instead", () => {
  const readied: string[] = [];
  storesHere(async (id) => {
    readied.push(id);
  });
  const before = Socket.made.length;
  startSession("d", new AbortController().signal);
  assert.deepEqual(readied, ["d"]);
  assert.equal(Socket.made.length, before);
});
