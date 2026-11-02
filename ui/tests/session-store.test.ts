import assert from "node:assert/strict";
import { test } from "node:test";

class Socket {
  static OPEN = 1;
  static CONNECTING = 0;
  static CLOSED = 3;
  static all: Socket[] = [];
  readyState = Socket.OPEN;
  url: string;
  onmessage?: (ev: { data: string | ArrayBuffer }) => void;
  onclose?: () => void;
  constructor(url?: string) { this.url = url ?? ""; Socket.all.push(this); }
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

await test("a reconnect resumes from what's shown, a resume adds to it, a snapshot replaces it", async () => {
  const store = new SessionStore("resume");
  const off = store.subscribe(() => {});
  const ids = () => store.get().events.map((e) => e.id);
  try {
    const ws = Socket.all.at(-1)!;
    ws.frame({ t: "open", info: { id: "resume", status: "running" }, rev: 10, born: 77 });
    ws.frame({ t: "state", state });
    ws.frame({ t: "events", events: [event(1), event(2)] });
    ws.frame({ t: "synced" });
    await tick();
    ws.frame({ t: "events", events: [event(3)], rev: 11 });
    await tick();
    assert.deepEqual(ids(), ["e1", "e2", "e3"]);
    // Away: it asks for only what came after what it shows.
    ws.readyState = Socket.CLOSED;
    store.wake();
    const again = Socket.all.at(-1)!;
    assert.notEqual(again, ws);
    assert.match(again.url, /[?&]after=77\.11(&|$)/);
    again.frame({ t: "resumed", info: { id: "resume", status: "running" }, rev: 14, born: 77 });
    again.frame({ t: "state", state });
    again.frame({ t: "events", events: [{ ...event(3), text: "changed" }, event(4)] });
    again.frame({ t: "synced" });
    await tick();
    assert.deepEqual(ids(), ["e1", "e2", "e3", "e4"], "the resume adds to what's shown, none twice");
    const e3 = store.get().events.find((e) => e.id === "e3");
    assert.equal(e3?.kind === "assistant" && e3.text, "changed");
    // A resume cut off before its synced isn't what's shown: the next asks from before it.
    again.readyState = Socket.CLOSED;
    store.wake();
    const cut = Socket.all.at(-1)!;
    assert.match(cut.url, /[?&]after=77\.14(&|$)/);
    cut.frame({ t: "resumed", info: { id: "resume", status: "running" }, rev: 20, born: 77 });
    cut.readyState = Socket.CLOSED;
    cut.onclose?.();
    store.wake();
    const next = Socket.all.at(-1)!;
    assert.match(next.url, /[?&]after=77\.14(&|$)/);
    // A snapshot (another server, or one that can't resume) still replaces what's shown.
    for (const frame of [{ t: "open", info: { id: "resume", status: "running" }, rev: 3, born: 91 }, { t: "state", state }, { t: "events", events: [event(9)] }, { t: "synced" }]) next.frame(frame);
    await tick();
    assert.deepEqual(ids(), ["e9"]);
  } finally { off(); }
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

await test("a message's id is a UUIDv7 made once: 32 hex digits, its time first, and never another's (CP-T3.2)", async () => {
  const { clientId } = await import("../src/api/harness.ts");
  const ids = Array.from({ length: 10_000 }, () => clientId());
  for (const id of ids.slice(0, 100)) assert.match(id, /^[0-9a-f]{12}7[0-9a-f]{3}[89ab][0-9a-f]{15}$/);
  assert.equal(new Set(ids).size, ids.length);
  const ms = Number.parseInt(ids[0]!.slice(0, 12), 16);
  assert.ok(Math.abs(ms - Date.now()) < 60_000, `its time: ${ms}`);
  assert.ok(ids.every((id, i) => i === 0 || id.slice(0, 12) >= ids[i - 1]!.slice(0, 12)), "made later, sorted later");
});

await test("a changeset comes whole, then as what changed in it, taken in place in the server's order", async () => {
  const file = (path: string, added: number, tree?: string) => ({ path, ...(tree ? { tree } : {}), added, removed: 0, hunks: [], at: 1 });
  const store = new SessionStore("changes");
  const off = store.subscribe(() => {});
  const ws = Socket.all.at(-1)!;
  try {
    assert.match(ws.url, /patch=changes/, "the socket asks for changesets as patches");
    for (const frame of snapshot("changes", [])) ws.frame(frame);
    ws.frame({ t: "changes", files: [file("a.rs", 1), file("b.rs", 1), file("a.rs", 1, "/w")] });
    await tick();
    const kept = store.get().changes?.[0];
    // The file the agent touched in place, the one it put back gone, a new one after the rest.
    ws.frame({ t: "changes_patch", set: [file("a.rs", 7, "/w"), file("c.rs", 2)], gone: [{ path: "b.rs" }] });
    await tick();
    assert.deepEqual(store.get().changes, [file("a.rs", 1), file("a.rs", 7, "/w"), file("c.rs", 2)]);
    assert.equal(store.get().changes?.[0], kept, "a file that didn't change stays the same object");
  } finally { off(); }
});
