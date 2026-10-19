import assert from "node:assert/strict";
import { test } from "node:test";

class Socket {
  static OPEN = 1;
  static CONNECTING = 0;
  static CLOSED = 3;
  static all: Socket[] = [];
  readyState = Socket.OPEN;
  onmessage?: (ev: { data: string | ArrayBuffer }) => void;
  onclose?: () => void;
  constructor() { Socket.all.push(this); }
  send() {}
  close() { this.readyState = Socket.CLOSED; this.onclose?.(); }
  frame(value: unknown) { this.onmessage?.({ data: JSON.stringify(value) }); }
  binary(text: string) { this.onmessage?.({ data: new TextEncoder().encode(text).buffer }); }
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
const tick = () => new Promise<void>((resolve) => setImmediate(resolve));
const event = (seq: number) => ({ id: `e${seq}`, seq, at: seq, kind: "assistant", text: "text" });
const state = { status: "running", tasks: [], delivery: {}, dropped: 0 };
const snapshot = (id: string, events: unknown[]) => [
  { t: "open", info: { id, status: "running" } },
  { t: "state", state },
  { t: "events", events },
  { t: "synced" },
];

await test("live history stays bounded and retained events still update in place", async () => {
  const store = new SessionStore("history");
  const off = store.subscribe(() => {});
  const ws = Socket.all.at(-1)!;
  try {
    for (const frame of snapshot("history", Array.from({ length: 4000 }, (_, i) => event(i)))) ws.frame(frame);
    await tick();
    ws.frame({ t: "events", events: Array.from({ length: 4000 }, (_, i) => event(i + 4000)) });
    ws.frame({ t: "state", state: { ...state, dropped: 4000 } });
    await tick();
    assert.equal(store.get().events.length, 4000);
    assert.equal(store.get().events[0]?.id, "e4000");
    ws.frame({ t: "stream", id: "e7999", text: " appended" });
    ws.frame({ t: "events", events: [{ ...event(4500), text: "replaced" }] });
    await tick();
    assert.equal(store.get().events.find((e) => e.id === "e4500")?.kind === "assistant" && store.get().events.find((e) => e.id === "e4500")?.text, "replaced");
    assert.equal(store.get().events.at(-1)?.kind === "assistant" && store.get().events.at(-1)?.text, "text appended");
    assert.equal(new Set(store.get().events.map((e) => e.id)).size, 4000);
  } finally { off(); }
});

await test("a previous connection's unpack cannot let text overtake a new snapshot", async () => {
  const original = globalThis.DecompressionStream;
  const release: (() => void)[] = [];
  // Control completion of unpacking without depending on compression timing.
  globalThis.DecompressionStream = class {
    constructor() {
      const wait = new Promise<void>((resolve) => release.push(resolve));
      return new TransformStream({ async transform(chunk, controller) { await wait; controller.enqueue(chunk); } });
    }
  } as typeof DecompressionStream;
  const store = new SessionStore("reconnect");
  const off = store.subscribe(() => {});
  const packed = snapshot("reconnect", [event(1)]).map((f) => JSON.stringify(f)).join("\n");
  try {
    const old = Socket.all.at(-1)!;
    old.binary(packed);
    await tick();
    old.readyState = Socket.CLOSED;
    store.wake();
    const ws = Socket.all.at(-1)!;
    assert.notEqual(ws, old);
    ws.binary(packed);
    release[0]!();
    await tick();
    ws.frame({ t: "stream", id: "e1", text: " after snapshot" });
    assert.equal(release.length, 2);
    release[1]!();
    await tick();
    await tick();
    const last = store.get().events.at(-1);
    assert.equal(last?.kind === "assistant" && last.text, "text after snapshot");
  } finally {
    for (const resolve of release) resolve();
    off();
    globalThis.DecompressionStream = original;
  }
});

await test("cached resources do not process live events while nothing shows them", async () => {
  const { preload } = await import("../src/lib/resource.ts");
  const { events } = await import("../src/api/events.ts");
  let reads = 0;
  let applied = 0;
  preload("test/idle", async () => { reads++; return 0; }, {
    on: ["queue"],
    apply: (value) => { applied++; return value + 1; },
  });
  await tick();
  events.start();
  try {
    const ws = Socket.all.at(-1)!;
    ws.frame({ type: "queue", queued: 1, running: 1 });
    ws.frame({ type: "resync" });
    await new Promise<void>((resolve) => setTimeout(resolve, 300));
    assert.equal(reads, 1);
    assert.equal(applied, 0);
  } finally { events.stop(); }
});
