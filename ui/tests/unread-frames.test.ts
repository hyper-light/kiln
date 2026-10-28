import assert from "node:assert/strict";
import { test } from "node:test";

/** A browser's socket, open, keeping what's sent on it. */
class Socket {
  static OPEN = 1;
  static CONNECTING = 0;
  static CLOSED = 3;
  static all: Socket[] = [];
  readyState = Socket.OPEN;
  url: string;
  sent: string[] = [];
  onopen?: () => void;
  onmessage?: (ev: { data: string | ArrayBuffer }) => void;
  onclose?: () => void;
  onerror?: () => void;
  constructor(url?: string) {
    this.url = url ?? "";
    Socket.all.push(this);
  }
  send(text: string) {
    this.sent.push(text);
  }
  close() {
    this.readyState = Socket.CLOSED;
    this.onclose?.();
  }
  text(data: string) {
    this.onmessage?.({ data });
  }
  frame(value: unknown) {
    this.text(JSON.stringify(value));
  }
  said() {
    return this.sent.map((s) => JSON.parse(s) as { t?: string; of?: string; why?: string });
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
const { events } = await import("../src/api/events.ts");
const { readStreamFrame } = await import("../src/api/frames.ts");
const tick = () => new Promise<void>((resolve) => setImmediate(resolve));
const event = (seq: number) => ({ id: `e${seq}`, seq, at: seq, kind: "assistant", text: `line ${seq}` });

/** What a page says up its socket of the frames it couldn't read: an unknown tag named, one that isn't JSON not. */
function toldOf(sent: { t?: string; of?: string; why?: string }[]) {
  assert.equal(sent.length, 2, JSON.stringify(sent));
  const [unknown, garbled] = sent;
  assert.deepEqual(unknown, { t: "unread", of: "hologram", why: "unknown frame" });
  assert.equal(garbled?.t, "unread");
  assert.equal(garbled?.of, "");
  assert.match(garbled?.why ?? "", /^not JSON/);
}

await test("the session store applies no frame with an unknown tag or that isn't JSON, and says so up its socket", async () => {
  const store = new SessionStore("unread");
  const off = store.subscribe(() => {});
  try {
    const ws = Socket.all.at(-1)!;
    for (const f of [
      { t: "open", info: { id: "unread", status: "idle" } },
      { t: "state", state: { status: "idle", tasks: [], delivery: {}, dropped: 0 } },
      { t: "events", events: [event(1)] },
      { t: "synced" },
    ])
      ws.frame(f);
    await tick();
    const before = store.get();
    assert.deepEqual(
      before.events.map((e) => e.id),
      ["e1"],
    );
    // A frame of a newer server's (the plan's example), carrying what a known one would; and one cut short.
    ws.frame({ t: "hologram", events: [event(2)] });
    ws.text('{"t":"events","events":[');
    await tick();
    assert.equal(store.get(), before, "nothing applied");
    toldOf(ws.said());
    // The next frame that reads is applied as ever.
    ws.frame({ t: "events", events: [event(2)] });
    await tick();
    assert.deepEqual(
      store.get().events.map((e) => e.id),
      ["e1", "e2"],
    );
  } finally {
    off();
  }
});

await test("the events socket applies no frame with an unknown tag or that isn't JSON, and says so up its socket", async () => {
  const heard: unknown[] = [];
  const off = events.on((ev) => heard.push(ev));
  events.start();
  try {
    const ws = Socket.all.at(-1)!;
    assert.match(ws.url, /\/api\/events$/);
    ws.onopen?.();
    ws.frame({ type: "hologram", run: { id: "r1" } });
    ws.text("{not json");
    assert.deepEqual(heard, [], "nothing applied");
    toldOf(ws.said());
    // What the server couldn't read of what this page sent: nothing to apply.
    ws.frame({ type: "refused", of: "text", code: "malformed", why: "not JSON" });
    assert.deepEqual(heard, []);
    ws.frame({ type: "terminals" });
    assert.deepEqual(heard, [{ type: "terminals" }], "the next frame that reads is applied");
    assert.equal(ws.sent.length, 2, "and nothing more said");
  } finally {
    off();
    events.stop();
  }
});

await test("a live page's socket reads each frame by its one key, and tells what it can't of the rest", () => {
  // The server's, and what the stream's runner said, each of a kind the type has: read whole.
  for (const f of [{ webrtc: { can: true } }, { refused: { of: "text", code: "malformed", why: "not JSON" } }, { loaded: { title: "A" } }, { tabs: { list: [], shown: "", seq: 1 } }, { closed: { why: "it was closed" } }])
    assert.deepEqual(readStreamFrame(JSON.stringify(f)), { frame: f });
  // A kind the type doesn't have (the plan's example), named; one cut short, more than one key or
  // none, or not an object, not.
  assert.deepEqual(readStreamFrame(JSON.stringify({ hologram: { a: 1 } })), { unread: { t: "unread", of: "hologram", why: "unknown frame" } });
  for (const [text, why] of [
    ['{"loaded":{', /^not JSON/],
    ['{"loaded":{},"navigated":{}}', /^2 keys/],
    ["{}", /^0 keys/],
    ['["loaded"]', /^not a JSON object/],
  ] as const) {
    const read = readStreamFrame(text);
    assert.ok("unread" in read && read.unread.of === "" && why.test(read.unread.why), `${text}: ${JSON.stringify(read)}`);
  }
});
