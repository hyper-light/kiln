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
  constructor(url?: string) {
    this.url = url ?? "";
    Socket.all.push(this);
  }
  open() {
    this.readyState = Socket.OPEN;
    this.onopen?.();
  }
  send(text: string) {
    this.sent.push(text);
  }
  close() {
    this.readyState = Socket.CLOSED;
    this.onclose?.();
  }
  frame(value: unknown) {
    this.onmessage?.({ data: JSON.stringify(value) });
  }
  said() {
    return this.sent.map((s) => JSON.parse(s) as { t: string; id: string; c: { type: string; id?: string; text?: string } });
  }
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
const { SessionStore } = await import("../src/api/harness.ts");
type Store = InstanceType<typeof SessionStore>;
const tick = () => new Promise<void>((resolve) => setImmediate(resolve));
const user = (id: string, seq: number, text = "hello") => ({ id, seq, at: seq, kind: "user", text, client_id: id, origin: "human" });
const idle = { status: "idle", tasks: [], queue: [], delivery: {}, dropped: 0 };
/** The newest socket, open, with the session's snapshot on it as the server sends it on attach. */
function attach(store: Store, state: object = idle, events: unknown[] = []) {
  const ws = Socket.all.at(-1)!;
  if (ws.readyState !== Socket.OPEN) ws.open();
  for (const f of [{ t: "open", info: { id: store.id, status: "idle" } }, { t: "state", state }, { t: "events", events }, { t: "synced" }]) ws.frame(f);
  return ws;
}
const shown = (store: Store) => store.get().outgoing.map((o) => [o.id, o.state] as const);
const send = (store: Store, id: string, extra: object = {}) => store.send({ type: "send", id, text: `say ${id}`, mode: "queue", ...extra });

await test("a message shows as sending the moment it goes, as sent once its machine has it, and goes from there once the conversation has it", async () => {
  const store = new SessionStore("at-once");
  const off = store.subscribe(() => {});
  try {
    const ws = attach(store);
    await tick();
    assert.equal(send(store, "m1", { text: "port the parser", attachments: [{ name: "a.png", mime: "image/png", file_id: "f1" }], reply_to: "e0" }), true);
    await tick();
    const [o] = store.get().outgoing;
    assert.deepEqual([o?.id, o?.state, o?.text, o?.attachments, o?.reply_to], ["m1", "sending", "port the parser", ["a.png"], "e0"]);
    const went = ws.said().at(-1);
    assert.deepEqual([went?.t, went?.id, went?.c.id], ["cmd", "m1", "m1"], "it goes under its own id");
    ws.frame({ t: "ack", id: "m1", outcome: "taken" });
    await tick();
    assert.deepEqual(shown(store), [["m1", "sent"]], "its machine has it: sent, until the conversation shows it");
    ws.frame({ t: "events", events: [user("m1", 1, "port the parser")] });
    await tick();
    assert.deepEqual(shown(store), [], "the conversation shows it now: it isn't shown twice");
    assert.equal(store.get().events.at(-1)?.id, "m1");
  } finally {
    off();
  }
});

await test("a message the agent queues goes from there once its queue (or its subagent's) holds it", async () => {
  const store = new SessionStore("queued");
  const off = store.subscribe(() => {});
  try {
    const ws = attach(store, { ...idle, status: "running" });
    await tick();
    send(store, "m2");
    send(store, "m3", { task: "t1" });
    await tick();
    assert.deepEqual(shown(store), [
      ["m2", "sending"],
      ["m3", "sending"],
    ]);
    assert.equal(store.get().outgoing[1]?.task, "t1");
    ws.frame({ t: "state_patch", set: { queue: [{ id: "m2", text: "say m2", at: 1 }] } });
    await tick();
    assert.deepEqual(shown(store), [["m3", "sending"]]);
    ws.frame({ t: "state_patch", set: { tasks: [{ id: "t1", description: "sub", status: "running", started: 1, queue: [{ id: "m3", text: "say m3", at: 2 }] }] } });
    await tick();
    assert.deepEqual(shown(store), []);
  } finally {
    off();
  }
});

await test("one refused for good says it failed to send, in the server's words; one refused for now, or parked, stays on its way", async () => {
  const store = new SessionStore("refused");
  const off = store.subscribe(() => {});
  try {
    const ws = attach(store);
    await tick();
    send(store, "m4");
    send(store, "m5");
    send(store, "m6");
    ws.frame({ t: "nack", id: "m4", code: "no_session", why: "this session is gone", again: false });
    ws.frame({ t: "nack", id: "m5", code: "unreachable", why: "the session's machine is not connected", again: true });
    ws.frame({ t: "ack", id: "m6", outcome: "parked" });
    await tick();
    assert.deepEqual(shown(store), [
      ["m4", "failed"],
      ["m5", "sending"],
      ["m6", "sending"],
    ]);
    assert.equal(store.get().outgoing[0]?.why, "this session is gone");
  } finally {
    off();
  }
});

await test("a message sent on a socket that drops goes again under its id on the next, and shows as sending until the conversation has it", async () => {
  const store = new SessionStore("dropped");
  const off = store.subscribe(() => {});
  try {
    const ws = attach(store);
    await tick();
    send(store, "m7");
    ws.close();
    store.wake();
    const again = Socket.all.at(-1)!;
    assert.notEqual(again, ws, "it reconnected");
    again.open();
    assert.deepEqual(
      again.said().map((f) => [f.t, f.id]),
      [["cmd", "m7"]],
      "sent again, under the same id",
    );
    attach(store);
    await tick();
    assert.deepEqual(shown(store), [["m7", "sending"]]);
    again.frame({ t: "events", events: [user("m7", 1)] });
    await tick();
    assert.deepEqual(shown(store), []);
  } finally {
    off();
  }
});

await test("Retry sends a message refused for good again as a new message; Remove lets it go; a retry while the socket is down sends nothing", async () => {
  const store = new SessionStore("retry");
  const off = store.subscribe(() => {});
  try {
    const ws = attach(store);
    await tick();
    send(store, "m9", { text: "again" });
    assert.equal(store.resend("m9"), false, "on its way still: not sent a second time");
    ws.frame({ t: "nack", id: "m9", code: "no_session", why: "this session is gone", again: false });
    await tick();
    const before = ws.sent.length;
    assert.equal(store.resend("m9"), true);
    await tick();
    assert.equal(ws.sent.length, before + 1);
    const again = ws.said().at(-1);
    assert.ok(again && again.id !== "m9" && again.c.id === again.id, "under an id of its own: the refused one's answer stays the refusal");
    assert.deepEqual([again?.c.type, again?.c.text], ["send", "again"], "its words");
    assert.deepEqual(shown(store), [[again?.id, "sending"]], "the new one in its place");
    ws.frame({ t: "nack", id: again?.id, code: "no_session", why: "this session is gone", again: false });
    await tick();
    store.discard(again?.id ?? "");
    await tick();
    assert.deepEqual(shown(store), []);
    send(store, "m10");
    ws.frame({ t: "nack", id: "m10", code: "no_session", why: "no", again: false });
    ws.readyState = Socket.CLOSED;
    const sent = ws.sent.length;
    assert.equal(store.resend("m10"), false);
    assert.equal(ws.sent.length, sent);
    assert.deepEqual(shown(store), [["m10", "failed"]]);
  } finally {
    off();
  }
});

await test("the agent ending fails what's still on its way but not what its machine took, and a message already in the conversation isn't shown again", async () => {
  const store = new SessionStore("ended");
  const off = store.subscribe(() => {});
  try {
    const ws = attach(store, idle, [user("m11", 1)]);
    await tick();
    send(store, "m11");
    await tick();
    assert.deepEqual(shown(store), [], "sent again under its id, it's the one shown");
    send(store, "m12");
    send(store, "m13");
    ws.frame({ t: "ack", id: "m13", outcome: "accepted" });
    ws.frame({ t: "closed" });
    await tick();
    assert.deepEqual(shown(store), [
      ["m12", "failed"],
      ["m13", "sent"],
    ]);
    assert.match(store.get().outgoing[0]?.why ?? "", /ended/i);
  } finally {
    off();
  }
});

await test("CP-T3.2: Retry sends a message that may have got there (no answer came, or its agent ended first) under its own id, so its inbox takes it once", async () => {
  const store = new SessionStore("maybe");
  const off = store.subscribe(() => {});
  const realNow = Date.now;
  try {
    const ws = attach(store);
    await tick();
    // The server's own word that no answer came: it may have been applied.
    send(store, "m14");
    ws.frame({ t: "nack", id: "m14", code: "unanswered", why: "its runner never answered", again: false });
    await tick();
    assert.deepEqual(shown(store), [["m14", "failed"]]);
    assert.equal(store.resend("m14"), true);
    await tick();
    const again = ws.said().at(-1);
    assert.deepEqual([again?.id, again?.c.id], ["m14", "m14"], "under its own id: a second prompt never, its inbox answering a repeat");
    assert.deepEqual(shown(store), [["m14", "sending"]], "on its way again, in its place");
    // Its agent ended before it got there: it may have, all the same.
    send(store, "m15");
    ws.frame({ t: "closed" });
    await tick();
    assert.deepEqual(
      shown(store).filter(([id]) => id === "m15"),
      [["m15", "failed"]],
    );
    assert.equal(store.resend("m15"), true);
    assert.deepEqual([ws.said().at(-1)?.id, ws.said().at(-1)?.c.id], ["m15", "m15"]);
  } finally {
    off();
  }
  // No answer at all for longer than a command waits for one: this page gives up on it, but not on its id.
  const later = new SessionStore("unheard");
  const done = later.subscribe(() => {});
  try {
    const ws = attach(later);
    await tick();
    send(later, "m16");
    ws.close();
    Date.now = () => realNow() + 11 * 60_000;
    later.wake();
    const next = attach(later);
    await tick();
    assert.notEqual(next, ws, "it reconnected");
    assert.deepEqual(shown(later), [["m16", "failed"]], "no answer came");
    assert.equal(later.resend("m16"), true);
    assert.deepEqual([next.said().at(-1)?.id, next.said().at(-1)?.c.id], ["m16", "m16"]);
  } finally {
    Date.now = realNow;
    done();
  }
});

await test("CP-T3.2: a message its inbox refuses as `reused` got there under its id before (sent again with its upload gone, it read as another): it shows as sent, and Retry never makes it a new message", async () => {
  const store = new SessionStore("reused");
  const off = store.subscribe(() => {});
  try {
    const ws = attach(store);
    await tick();
    send(store, "m20", { text: "look at this", attachments: [{ name: "shot.png", mime: "image/png", file_id: "f9" }] });
    ws.frame({ t: "nack", id: "m20", code: "reused", why: "message id m20 was already used for another message to session reused", again: false });
    await tick();
    assert.deepEqual(shown(store), [["m20", "sent"]], "it got there: sent, until the conversation shows it");
    const before = ws.sent.length;
    assert.equal(store.resend("m20"), false, "nothing to send again");
    await tick();
    assert.equal(ws.sent.length, before, "and nothing went");
  } finally {
    off();
  }
});
