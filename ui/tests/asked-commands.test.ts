import assert from "node:assert/strict";
import { test } from "node:test";

/** A browser's socket, opened when the test says, keeping what's sent on it. */
class Socket {
  static OPEN = 1;
  static CONNECTING = 0;
  static CLOSED = 3;
  static all: Socket[] = [];
  readyState = Socket.CONNECTING;
  url: string;
  sent: string[] = [];
  onopen?: () => void;
  onmessage?: (ev: { data: string | ArrayBuffer }) => void;
  onclose?: () => void;
  constructor(url?: string) { this.url = url ?? ""; Socket.all.push(this); }
  open() { this.readyState = Socket.OPEN; this.onopen?.(); }
  send(text: string) { this.sent.push(text); }
  close() { this.readyState = Socket.CLOSED; this.onclose?.(); }
  frame(value: unknown) { this.onmessage?.({ data: JSON.stringify(value) }); }
  said() { return this.sent.map((s) => JSON.parse(s) as { t: string; id: string; c: unknown }); }
}

Object.assign(globalThis, {
  WebSocket: Socket,
  location: { protocol: "http:", host: "localhost" },
  document: new EventTarget(),
  window: Object.assign(new EventTarget(), {
    setTimeout: (fn: () => void, ms: number) => setTimeout(fn, ms).unref(),
    clearTimeout,
  }),
  requestAnimationFrame: (fn: () => void) => setImmediate(fn),
  cancelAnimationFrame: clearImmediate,
});
const { SessionStore, ASKED_FOR_MS } = await import("../src/api/harness.ts");
const tick = () => new Promise<void>((resolve) => setImmediate(resolve));
const attached = (ws: Socket, id: string) => {
  for (const f of [{ t: "open", info: { id, status: "idle" } }, { t: "state", state: { status: "idle", tasks: [], delivery: {}, dropped: 0 } }, { t: "events", events: [] }, { t: "synced" }]) ws.frame(f);
};

await test("a command waits for its ack or nack, and goes again once on the next socket under its id", async () => {
  const store = new SessionStore("asked");
  const off = store.subscribe(() => {});
  try {
    const ws = Socket.all.at(-1)!;
    ws.open();
    attached(ws, "asked");
    await tick();
    assert.equal(store.send({ type: "set_model", model: "m1" }), true);
    const [first] = ws.said();
    assert.equal(first?.t, "cmd");
    assert.deepEqual(first?.c, { type: "set_model", model: "m1" });
    const id = first!.id;
    assert.ok(id, "it goes under an id");
    await tick();
    assert.deepEqual(store.get().asked.map((a) => [a.id, a.state]), [[id, "pending"]]);

    // Its socket drops before its answer: the next one sends it again, once, under its id.
    ws.close();
    store.wake();
    const next = Socket.all.at(-1)!;
    assert.notEqual(next, ws, "a new socket");
    next.open();
    assert.deepEqual(next.said(), [{ t: "cmd", id, c: { type: "set_model", model: "m1" } }]);
    attached(next, "asked");
    await tick();
    assert.deepEqual(store.get().asked.map((a) => a.state), ["pending"], "still waiting for its answer");

    // Its answer: no longer waiting, and not sent again on the socket after.
    next.frame({ t: "ack", id, outcome: "taken" });
    await tick();
    assert.deepEqual(store.get().asked, []);
    next.close();
    store.wake();
    const third = Socket.all.at(-1)!;
    third.open();
    assert.deepEqual(third.said(), [], "nothing left to send again");
    attached(third, "asked");

    // A refusal shows, saying how and why.
    store.send({ type: "set_effort", effort: "max" });
    const refused = third.said().at(-1)!.id;
    third.frame({ t: "nack", id: refused, code: "refused", why: "its runner refused it: no such effort" });
    await tick();
    assert.deepEqual(store.get().asked.map((a) => [a.id, a.state, a.code, a.why]), [[refused, "refused", "refused", "its runner refused it: no such effort"]]);
    assert.equal(store.get().error, "its runner refused it: no such effort");

    // A message goes under its own id, which its inbox takes once. Not through this time (its
    // machine away), it waits, and goes again on the next socket; parked, likewise.
    store.send({ type: "send", id: "m-1", text: "hello" });
    assert.equal(third.said().at(-1)?.id, "m-1");
    third.frame({ t: "nack", id: "m-1", code: "unreachable", why: "the session's machine is not connected", again: true });
    await tick();
    assert.ok(store.get().asked.some((a) => a.id === "m-1" && a.state === "pending" && a.why === "the session's machine is not connected"), "not through yet isn't refused");
    // Its inbox not having said yet, it waits too; to a runner without inboxes, it's done with.
    store.send({ type: "send", id: "m-2", text: "again?" });
    third.frame({ t: "ack", id: "m-2", outcome: "pending" });
    store.send({ type: "send", id: "m-3", text: "unchecked" });
    third.frame({ t: "ack", id: "m-3", outcome: "unchecked" });
    await tick();
    assert.ok(store.get().asked.some((a) => a.id === "m-2" && a.state === "pending" && a.why?.includes("inbox")), "pending waits");
    assert.ok(!store.get().asked.some((a) => a.id === "m-3"), "unchecked is final");
    store.send({ type: "approve", id: "a-1", decision: "allow" });
    const approve = third.said().at(-1)!.id;
    third.frame({ t: "ack", id: approve, outcome: "parked" });
    await tick();
    assert.ok(store.get().asked.some((a) => a.id === approve && a.state === "pending" && a.why?.includes("away")), "parked waits");
    // And one not answered at all.
    store.send({ type: "interrupt" });
    const unheard = third.said().at(-1)!.id;
    third.close();
    store.wake();
    const fourth = Socket.all.at(-1)!;
    // Long after: what was answered for now goes on; what had no answer is said to have had none.
    const realNow = Date.now;
    Date.now = () => realNow() + ASKED_FOR_MS + 1000;
    try {
      fourth.open();
    } finally {
      Date.now = realNow;
    }
    assert.ok(["m-1", "m-2", approve].every((id) => fourth.said().some((f) => f.id === id)), "each sent again under its id");
    assert.ok(!fourth.said().some((f) => f.id === "m-3"), "never a second prompt");
    assert.ok(!fourth.said().some((f) => f.id === unheard), "not sent again");
    assert.deepEqual(store.get().asked.filter((a) => a.id === unheard).map((a) => [a.state, a.code]), [["refused", "unanswered"]]);
    attached(fourth, "asked");
    fourth.frame({ t: "ack", id: "m-1", outcome: "accepted" });
    fourth.frame({ t: "ack", id: "m-2", outcome: "accepted", accepted: { repeat: true } });
    fourth.frame({ t: "ack", id: approve, outcome: "taken" });
    await tick();
    assert.ok(!store.get().asked.some((a) => ["m-1", "m-2", approve].includes(a.id)), "its inbox took the messages, its runner the other");

    // Without a socket: not sent, and nothing waits for an answer.
    fourth.close();
    const before = store.get().asked.length;
    assert.equal(store.send({ type: "interrupt" }), false);
    await tick();
    assert.equal(store.get().asked.length, before);
  } finally {
    off();
  }
});
