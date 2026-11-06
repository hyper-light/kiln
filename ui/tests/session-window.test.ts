// W21-R15 (an agent's page opens at once): a window of a session's events, its older pages and
// its lean events' bodies, as the session's store takes them (docs/wire.md, "Windows").
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
  constructor(url?: string) {
    this.url = url ?? "";
    Socket.all.push(this);
  }
  send() {}
  close() {
    this.readyState = Socket.CLOSED;
    this.onclose?.();
  }
  frame(value: unknown) {
    this.onmessage?.({ data: JSON.stringify(value) });
  }
}

/** What the server answers each path with, and the paths asked for, in order. */
const answers = new Map<string, () => unknown>();
const asked: string[] = [];

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
  fetch: async (path: string) => {
    asked.push(path);
    const answer = answers.get(path);
    if (!answer) return new Response(JSON.stringify({ error: "not found" }), { status: 404 });
    return new Response(JSON.stringify(answer()), { status: 200, headers: { "content-type": "application/json" } });
  },
});
const { SessionStore, heldOf } = await import("../src/api/harness.ts");
const tick = () => new Promise<void>((resolve) => setImmediate(resolve));
const note = (seq: number, text = "text") => ({ id: `e${seq}`, seq, at: seq, kind: "assistant", text });
const user = (seq: number) => ({ id: `e${seq}`, seq, at: seq, kind: "user", text: "do it" });
const cmd = (seq: number, lean: boolean) => ({
  id: `e${seq}`,
  seq,
  at: seq,
  kind: "tool",
  tool_id: `t${seq}`,
  name: "Bash",
  tool: "shell",
  title: "cargo test",
  input: lean ? null : { command: "cargo test" },
  status: "done",
  exit_code: 0,
  started: seq,
  ended: seq + 1,
  ...(lean ? { held: { output: 11, input: 24, lines: 0 } } : { output: "all passed\n" }),
});
const state = { status: "idle", tasks: [], delivery: {}, dropped: 0 };
const seqs = (s: InstanceType<typeof SessionStore>) => s.get().events.map((e) => e.seq);
const range = (from: number, to: number) => Array.from({ length: to - from + 1 }, (_, i) => from + i);

/** A store whose socket sent a window of events `from`..`to`, `more` before it. */
async function opened(id: string, from: number, to: number, more: boolean, make = (s: number) => (s % 5 === 1 ? user(s) : note(s))) {
  const store = new SessionStore(id);
  const off = store.subscribe(() => {});
  const ws = Socket.all.at(-1)!;
  assert.match(ws.url, /[?&]window=1(&|$)/);
  assert.match(ws.url, /[?&]lean=1(&|$)/);
  ws.frame({ t: "open", info: { id, status: "idle" }, rev: 5, born: 9, window: { before: from, more } });
  ws.frame({ t: "state", state });
  ws.frame({ t: "events", events: range(from, to).map(make) });
  ws.frame({ t: "synced" });
  await tick();
  return { store, off, ws };
}

await test("w21_r15 a window says there's more, and older pages join before it in order, none twice", async () => {
  const { store, off, ws } = await opened("win", 41, 50, true);
  try {
    assert.equal(store.get().more, true);
    assert.deepEqual(seqs(store), range(41, 50));
    // A live change to an event from before the window that isn't held waits for its page.
    ws.frame({ t: "events", events: [note(7, "changed while away")], rev: 6 });
    await tick();
    assert.deepEqual(seqs(store), range(41, 50), "nothing alone above a gap");
    answers.set("/api/harness/sessions/win/timeline?before=41&lean=1", () => ({ events: range(21, 40).map(note), before: 21, more: true }));
    answers.set("/api/harness/sessions/win/timeline?before=21&lean=1", () => ({ events: [...range(1, 20).map(note), note(21)], before: 1, more: false }));
    await store.older();
    await tick();
    assert.deepEqual(seqs(store), range(21, 50));
    assert.equal(store.get().more, true);
    // New events while the next page comes; and a page that repeats one held keeps the one held.
    const page = store.older();
    ws.frame({ t: "events", events: [note(51)], rev: 7 });
    await page;
    await tick();
    assert.deepEqual(seqs(store), range(1, 51), "every event once, in order");
    assert.equal(store.get().more, false);
    const was = asked.length;
    await store.older();
    assert.equal(asked.length, was, "nothing more to ask for");
    // Held now (its page came), a live change to an old one is taken.
    ws.frame({ t: "events", events: [note(7, "changed")], rev: 8 });
    await tick();
    const e7 = store.get().events.find((e) => e.seq === 7);
    assert.equal(e7?.kind === "assistant" && e7.text, "changed");
  } finally {
    off();
  }
});

await test("w21_r15 a page asked for before a new window came isn't taken into it", async () => {
  const { store, off, ws } = await opened("again", 31, 40, true);
  try {
    answers.set("/api/harness/sessions/again/timeline?before=31&lean=1", () => ({ events: range(11, 30).map(note), before: 11, more: true }));
    // A reconnect that can't resume: a new window, beginning elsewhere.
    ws.readyState = Socket.CLOSED;
    const page = store.older();
    store.wake();
    const next = Socket.all.at(-1)!;
    next.frame({ t: "open", info: { id: "again", status: "idle" }, rev: 2, born: 10, window: { before: 36, more: true } });
    next.frame({ t: "events", events: range(36, 45).map(note) });
    next.frame({ t: "synced" });
    await page;
    await tick();
    assert.deepEqual(seqs(store), range(36, 45));
  } finally {
    off();
  }
});

await test("w21_r15 a server from before windows sends them all, and nothing is asked for", async () => {
  const store = new SessionStore("old");
  const off = store.subscribe(() => {});
  try {
    const ws = Socket.all.at(-1)!;
    ws.frame({ t: "open", info: { id: "old", status: "idle" }, rev: 5, born: 9 });
    ws.frame({ t: "events", events: range(1, 10).map(note) });
    ws.frame({ t: "synced" });
    await tick();
    assert.equal(store.get().more, false);
    // Any event, however old, is taken.
    ws.frame({ t: "events", events: [note(0)], rev: 6 });
    await tick();
    assert.deepEqual(seqs(store), range(0, 10));
    const was = asked.length;
    await store.older();
    assert.equal(asked.length, was);
  } finally {
    off();
  }
});

await test("w21_r15 a lean call's body comes once, asked for when it's shown, and takes its place", async () => {
  const { store, off, ws } = await opened("lean", 1, 4, false, (s) => (s === 1 ? user(s) : cmd(s, s === 3)));
  try {
    const lean = store.get().events.find((e) => e.seq === 3)!;
    assert.ok(heldOf(lean), "it came lean");
    assert.equal(heldOf(store.get().events.find((e) => e.seq === 2)!), undefined);
    answers.set("/api/harness/sessions/lean/events/e3?seq=3", () => cmd(3, false));
    const p = store.body(lean);
    assert.equal(store.body(lean), p, "one ask, the same promise, while it's what's held");
    const got = await p;
    await tick();
    assert.equal(typeof got === "object" && got.kind === "tool" && got.output, "all passed\n");
    const now = store.get().events.find((e) => e.seq === 3)!;
    assert.equal(heldOf(now), undefined, "whole in its place");
    assert.equal(now.kind === "tool" && now.output, "all passed\n");
    assert.equal(asked.filter((a) => a.includes("/events/e3")).length, 1);
    // One that came again lean (it changed) is asked for again.
    ws.frame({ t: "events", events: [{ ...cmd(3, true), title: "cargo test -p kiln" }], rev: 9 });
    await tick();
    const again = store.get().events.find((e) => e.seq === 3)!;
    assert.ok(heldOf(again));
    assert.notEqual(store.body(again), p);
    // What can't be had is said, not thrown.
    const four = { ...cmd(4, true), id: "gone" };
    assert.equal(typeof (await store.body(four)), "string");
  } finally {
    off();
  }
});

await test("w21_r15 find's whole asks for every lean call's body in pages, oldest held last", async () => {
  const { store, off } = await opened("find", 1, 6, false, (s) => (s === 1 ? user(s) : cmd(s, s % 2 === 0)));
  try {
    answers.set("/api/harness/sessions/find/timeline?before=7", () => ({ events: [cmd(5, false), cmd(6, false)], before: 5, more: true }));
    answers.set("/api/harness/sessions/find/timeline?before=5", () => ({ events: [user(1), cmd(2, false), cmd(3, false), cmd(4, false)], before: 1, more: false }));
    await store.whole();
    await tick();
    assert.equal(store.get().events.filter((e) => heldOf(e)).length, 0, "every one whole");
    assert.deepEqual(seqs(store), range(1, 6));
  } finally {
    off();
  }
});
