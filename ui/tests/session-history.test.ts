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
Object.assign(globalThis, {
  WebSocket: Socket,
  location: { protocol: "http:", host: "localhost" },
  document: new EventTarget(),
  window: Object.assign(new EventTarget(), { setTimeout: (fn: () => void, ms: number) => setTimeout(fn, ms).unref(), clearTimeout }),
  requestAnimationFrame: (fn: () => void) => setImmediate(fn),
  cancelAnimationFrame: clearImmediate,
});
const { SessionStore } = await import("../src/api/harness.ts");
const tick = () => new Promise<void>((r) => setImmediate(r));
const event = (seq: number, text = `text ${seq}`) => ({ id: `e${seq}`, seq, at: seq, kind: "assistant", text });
const cursor = "77.1.history";
const meta = (before: number | null, has_more = true) => ({ cursor, before, has_more, oversized: false });
function open(store: InstanceType<typeof SessionStore>, events = [event(10), event(11)]) {
  const off = store.subscribe(() => {});
  const ws = Socket.all.at(-1)!;
  assert.match(ws.url, /recent_turns=8&event_bytes=262144&first_bytes=16384/);
  ws.frame({ t: "open", info: { id: "history", status: "running" }, window: { before: 10, more: true }, rev: 10, born: 77 });
  ws.frame({ t: "history", ...meta(10) });
  ws.frame({ t: "events", events });
  assert.equal(store.get().ready, false, "bounded snapshots wait for their atomic synced boundary");
  ws.frame({ t: "synced" });
  return { ws, off };
}
function pendingFetch() {
  let finish: (body: unknown, status?: number) => void = () => {};
  let requests = 0;
  let signal: AbortSignal | undefined;
  globalThis.fetch = async (_url, init) => {
    requests++;
    signal = init?.signal ?? undefined;
    return new Promise<Response>((r) => {
      finish = (body, status = 200) => r(new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } }));
    });
  };
  return { finish: (body: unknown, status?: number) => finish(body, status), count: () => requests, aborted: () => signal?.aborted };
}

await test("W21-R02 older pages are single-flight and share canonical identities with live corrections", async () => {
  const store = new SessionStore("history");
  const { ws, off } = open(store);
  const held = store.event("e10");
  const fetch = pendingFetch();
  const p = store.loadOlder();
  assert.equal(store.loadOlder(), p);
  assert.equal(store.older(), p, "legacy and cursor controls share one canonical in-flight page");
  assert.equal(fetch.count(), 1);
  ws.frame({ t: "events", events: [event(5, "live correction")] });
  ws.frame({ t: "stream", id: "e5", text: " streamed" });
  fetch.finish({ events: [event(4), event(5, "stale disk")], history: meta(4) });
  await p;
  await tick();
  assert.deepEqual(
    store.get().events.map((e) => e.id),
    ["e4", "e5", "e10", "e11"],
  );
  assert.equal(store.event("e10"), held);
  assert.equal(store.event("e5")?.kind === "assistant" && store.event("e5")?.text, "live correction streamed");
  assert.equal(store.get().history?.before, 4);
  assert.equal(store.get().history?.loading, false);
  off();
  store.dispose();
});

await test("W21-R02 a displaced/deleted id cannot resurrect from an in-flight page", async () => {
  const store = new SessionStore("history");
  const { ws, off } = open(store);
  const fetch = pendingFetch();
  const p = store.loadOlder();
  ws.frame({ t: "events", events: [event(5), { ...event(5), id: "replacement", text: "new branch" }] });
  fetch.finish({ events: [event(4), event(5)], history: meta(4) });
  await p;
  await tick();
  assert.equal(store.event("e5"), undefined);
  assert.equal(store.event("replacement")?.kind === "assistant" && store.event("replacement")?.text, "new branch");
  assert.equal(store.get().events.filter((e) => e.seq === 5).length, 1);
  off();
  store.dispose();
});

await test("W21-R02 reset and logout cancel pages and cannot mix generations", async () => {
  for (const dispose of [false, true]) {
    const store = new SessionStore("history");
    const { ws, off } = open(store);
    const fetch = pendingFetch();
    const p = store.loadOlder();
    if (dispose) store.dispose();
    else {
      ws.frame({ t: "open", info: { id: "history", status: "running" }, rev: 20, born: 77 });
      ws.frame({ t: "history", ...meta(20), cursor: "77.20.history" });
      ws.frame({ t: "events", events: [event(20)] });
      ws.frame({ t: "synced" });
    }
    assert.equal(fetch.aborted(), true);
    fetch.finish({ events: [event(1)], history: meta(1, false) });
    await p;
    await tick();
    assert.equal(store.event("e1"), undefined);
    assert.deepEqual(
      store.get().events.map((e) => e.id),
      dispose ? [] : ["e20"],
    );
    off();
    store.dispose();
  }
});

await test("W21-R02 failed and invalid pages retain the current view and expose retry state", async () => {
  const store = new SessionStore("history");
  const { off } = open(store);
  for (const [body, status] of [
    [{ error: "offline" }, 503],
    [{ events: [event(1)], history: meta(10) }, 200],
  ] as const) {
    const fetch = pendingFetch();
    const p = store.loadOlder();
    fetch.finish(body, status);
    await p;
    await tick();
    assert.deepEqual(
      store.get().events.map((e) => e.id),
      ["e10", "e11"],
    );
    assert.equal(store.get().history?.loading, false);
    assert.ok(store.get().history?.error);
  }
  off();
  store.dispose();
});

await test("W21-R02 stale server cursors reconnect without an after revision and overflow retries boundedly", async () => {
  const store = new SessionStore("history");
  const { ws, off } = open(store);
  const fetch = pendingFetch();
  let p = store.loadOlder();
  fetch.finish({ error: "generation changed" }, 409);
  await p;
  assert.notEqual(Socket.all.at(-1), ws);
  assert.doesNotMatch(Socket.all.at(-1)!.url, /after=/);
  // Re-open to finish the bounded retry, then overflow the request-scoped live guards.
  const current = Socket.all.at(-1)!;
  current.frame({ t: "open", info: { id: "history" }, rev: 20, born: 77 });
  current.frame({ t: "history", ...meta(10) });
  current.frame({ t: "events", events: [event(10)] });
  current.frame({ t: "synced" });
  const more = pendingFetch();
  p = store.loadOlder();
  current.frame({ t: "events", events: Array.from({ length: 2050 }, (_, i) => event(i + 100)) });
  assert.equal(more.aborted(), true);
  more.finish({ events: [event(1)], history: meta(1) });
  await p;
  await tick();
  assert.equal(store.event("e1"), undefined);
  assert.match(store.get().history?.error ?? "", /Retry earlier/);
  off();
  store.dispose();
});

await test("W21-R02 resumed snapshots preserve loaded pages and legacy rolling trims advance the history boundary", async () => {
  const store = new SessionStore("history");
  const { ws, off } = open(store);
  const fetch = pendingFetch();
  const p = store.loadOlder();
  fetch.finish({ events: [event(1)], history: meta(1, false) });
  await p;
  await tick();
  const first = store.event("e1");
  ws.frame({ t: "resumed", info: { id: "history" }, rev: 12, born: 77 });
  ws.frame({ t: "events", events: [event(12)] });
  ws.frame({ t: "synced" });
  await tick();
  assert.equal(store.event("e1"), first);
  assert.equal(store.get().history?.hasMore, false);
  off();
  store.dispose();

  const rolling = new SessionStore("history");
  const opened = open(
    rolling,
    Array.from({ length: 4000 }, (_, i) => event(i + 10)),
  );
  opened.ws.frame({ t: "events", events: Array.from({ length: 4000 }, (_, i) => event(i + 4010)) });
  await tick();
  assert.equal(rolling.get().events.length, 4000);
  assert.equal(rolling.get().history?.before, 4010, "a newly evicted range remains discoverable by paging");
  assert.equal(rolling.get().history?.hasMore, true);
  opened.off();
  rolling.dispose();
});

await test("W21-R02 a page ahead of queued WS deltas never duplicates streaming text or resurrects an older id", async () => {
  const store = new SessionStore("history");
  const { ws, off } = open(store);
  const fetch = pendingFetch();
  const p = store.loadOlder();
  fetch.finish({ events: [{ ...event(5, "abc"), id: "replacement" }], history: meta(5), revision: 20 });
  await p;
  await tick();
  // A page says nothing about the socket's state/meta. It must not move global after=10.
  ws.readyState = Socket.CLOSED;
  store.wake();
  const resumed = Socket.all.at(-1)!;
  assert.match(resumed.url, /after=77\.10/);
  // A resume snapshot taken earlier than the HTTP overlay also preserves that row's barrier.
  resumed.frame({ t: "resumed", info: { id: "history" }, born: 77, rev: 18 });
  resumed.frame({ t: "events", events: [event(5, "obsolete branch"), { ...event(5, "a"), id: "replacement" }] });
  resumed.frame({ t: "synced" });
  resumed.frame({ t: "stream", id: "replacement", text: "b", rev: 19 });
  resumed.frame({ t: "stream", id: "replacement", text: "c", rev: 20 });
  resumed.frame({ t: "state", state: { status: "waiting", approvals: [], tasks: [], delivery: {}, dropped: 0 }, rev: 20 });
  resumed.frame({ t: "stream", id: "replacement", text: "d", rev: 21 });
  await tick();
  assert.equal(store.event("e5"), undefined);
  assert.equal(store.event("replacement")?.kind === "assistant" && store.event("replacement")?.text, "abcd");
  assert.equal(store.get().state?.status, "waiting", "the page barrier never discards unseen socket state");
  assert.equal(store.get().events.filter((e) => e.seq === 5).length, 1);
  off();
  store.dispose();
});

await test("W21-R02 a newer full event supersedes a page barrier and a reset never inherits it", async () => {
  const store = new SessionStore("history");
  const { ws, off } = open(store);
  const fetch = pendingFetch();
  const p = store.loadOlder();
  fetch.finish({ events: [event(5, "base")], history: meta(5), revision: 20 });
  await p;
  ws.frame({ t: "events", events: [{ ...event(5, "new full event"), id: "new-id" }], rev: 21 });
  ws.frame({ t: "stream", id: "new-id", text: "!", rev: 22 });
  await tick();
  assert.equal(store.event("e5"), undefined);
  assert.equal(store.event("new-id")?.kind === "assistant" && store.event("new-id")?.text, "new full event!");
  ws.frame({ t: "open", info: { id: "history" }, born: 88, rev: 1 });
  ws.frame({ t: "history", ...meta(5), cursor: "88.1.history" });
  ws.frame({ t: "events", events: [event(5, "new generation")] });
  ws.frame({ t: "synced" });
  ws.frame({ t: "stream", id: "e5", text: "!", rev: 2 });
  await tick();
  assert.equal(store.event("e5")?.kind === "assistant" && store.event("e5")?.text, "new generation!");
  assert.equal(store.event("new-id"), undefined);
  off();
  store.dispose();
});

await test("W21-R02 legacy unrevisioned page and stream frames retain exact append semantics", async () => {
  const store = new SessionStore("history");
  const { ws, off } = open(store);
  const fetch = pendingFetch();
  const p = store.loadOlder();
  fetch.finish({ events: [event(5, "base")], history: meta(5) });
  await p;
  ws.frame({ t: "stream", id: "e5", text: " next" });
  ws.frame({ t: "events", events: [event(5, "complete")] });
  ws.frame({ t: "stream", id: "e5", text: "!" });
  await tick();
  assert.equal(store.event("e5")?.kind === "assistant" && store.event("e5")?.text, "complete!");
  off();
  store.dispose();
});

await test("W21-R02 stale restore cleanup cannot cancel a newer history request", async () => {
  const store = new SessionStore("history");
  const { off } = open(store);
  const old = pendingFetch();
  const first = store.loadOlder();
  store.cancelOlder(first);
  assert.equal(old.aborted(), true);
  old.finish({ events: [event(1)], history: meta(1) });
  await first;
  const current = pendingFetch();
  const second = store.loadOlder();
  store.cancelOlder(first);
  assert.equal(current.aborted(), false, "cleanup must present the request identity it owns");
  current.finish({ events: [event(2)], history: meta(2) });
  await second;
  await tick();
  assert.equal(store.event("e1"), undefined);
  assert.equal(store.event("e2")?.id, "e2");
  off();
  store.dispose();
});

/** A snapshot that is its window's first part (`part`), as a server sends one to a viewer that takes a part ahead. */
function openPart(store: InstanceType<typeof SessionStore>, events = [event(10), event(11)]) {
  const off = store.subscribe(() => {});
  const ws = Socket.all.at(-1)!;
  ws.frame({ t: "open", info: { id: "history", status: "running" }, window: { before: 10, more: true }, rev: 10, born: 77 });
  ws.frame({ t: "history", ...meta(10), part: true });
  ws.frame({ t: "events", events });
  ws.frame({ t: "synced" });
  return { ws, off };
}

await test("W21-R02 a window comes in two parts: its newest first, the rest joining what's held", async () => {
  const store = new SessionStore("history");
  const { ws, off } = openPart(store);
  assert.equal(store.get().ready, true, "the first part is the snapshot: the conversation shows on it");
  assert.equal(store.get().history?.filling, true);
  assert.deepEqual(
    store.get().events.map((e) => e.id),
    ["e10", "e11"],
  );
  const held = store.event("e11");
  let settled = false;
  const filled = store.filled().then(() => (settled = true));
  // Nothing before the window is asked for until the window is whole.
  const fetch = pendingFetch();
  const older = store.loadOlder();
  await tick();
  assert.equal(fetch.count(), 0);
  assert.equal(settled, false);
  ws.frame({ t: "earlier", events: [event(6), event(7), event(11, "not over what's held")], ...meta(6) });
  await Promise.all([filled, older]);
  await tick();
  assert.deepEqual(
    store.get().events.map((e) => e.id),
    ["e6", "e7", "e10", "e11"],
  );
  assert.equal(store.event("e11"), held, "what's held stays the same object: nothing shown renders again");
  assert.deepEqual([store.get().history?.filling, store.get().history?.before, store.get().history?.hasMore, store.get().more], [false, 6, true, true]);
  assert.equal(store.filled(), store.filled(), "a whole window is filled already");
  // It's the window, not history its reader asked for: a long session still lets go of its oldest.
  ws.frame({ t: "events", events: Array.from({ length: 6000 }, (_, i) => event(i + 100)) });
  await tick();
  assert.equal(store.get().events.length, 4000);
  // And what's before it is a page away, as ever.
  const page = store.loadOlder();
  assert.equal(fetch.count(), 1);
  fetch.finish({ events: [], history: meta(null, false) });
  await page;
  off();
  store.dispose();
});

await test("W21-R02 a window's rest that never came is asked for again, the window whole this time", async () => {
  const store = new SessionStore("history");
  const { ws, off } = openPart(store);
  let looked = false;
  const waiting = store.filled().then(() => (looked = true));
  // Its socket closed before the rest came: a resume would bring what came after the part, never the rest. What's
  // shown is added to, so the window is asked for whole, no part of it ahead.
  ws.close();
  store.wake();
  const again = Socket.all.at(-1)!;
  assert.notEqual(again, ws);
  assert.doesNotMatch(again.url, /after=|first_bytes=/);
  again.frame({ t: "open", info: { id: "history", status: "running" }, window: { before: 8, more: true }, rev: 12, born: 77 });
  again.frame({ t: "history", ...meta(8) });
  again.frame({ t: "events", events: [event(8), event(10), event(11), event(12)] });
  again.frame({ t: "synced" });
  await waiting;
  assert.equal(looked, true, "whoever waited for the rest looks again at the snapshot that took its place");
  assert.deepEqual(
    store.get().events.map((e) => e.id),
    ["e8", "e10", "e11", "e12"],
  );
  assert.equal(store.get().history?.filling, false);
  // The rest of a snapshot that isn't what's held: not this window's.
  again.frame({ t: "earlier", events: [event(1)], ...meta(1, false) });
  await tick();
  assert.equal(store.event("e1"), undefined);
  // A whole window resumes as ever.
  again.close();
  store.wake();
  assert.match(Socket.all.at(-1)!.url, /after=77\.12/);
  assert.doesNotMatch(Socket.all.at(-1)!.url, /first_bytes=/);
  off();
  store.dispose();
});

await test("W21-R02 a first part that comes while a conversation is shown takes its place together with its rest", async () => {
  const store = new SessionStore("history");
  const shown: string[] = [];
  const { ws } = open(store, [event(8), event(9), event(10), event(11)]);
  const off = store.subscribe(() =>
    shown.push(
      store
        .get()
        .events.map((e) => e.id)
        .join(" "),
    ),
  );
  await tick();
  // A viewer that fell behind is sent a snapshot again on its socket, and its window's first part first.
  ws.frame({ t: "open", info: { id: "history", status: "running" }, window: { before: 11, more: true }, rev: 20, born: 77 });
  ws.frame({ t: "history", ...meta(11), cursor: "77.3.history", part: true });
  ws.frame({ t: "events", events: [event(11), event(12)] });
  ws.frame({ t: "synced" });
  await tick();
  assert.deepEqual(
    store.get().events.map((e) => e.id),
    ["e8", "e9", "e10", "e11"],
    "what's shown stays until the window that takes its place is whole",
  );
  ws.frame({ t: "earlier", events: [event(9), event(10)], ...meta(9), cursor: "77.3.history" });
  await tick();
  assert.deepEqual(
    store.get().events.map((e) => e.id),
    ["e9", "e10", "e11", "e12"],
  );
  assert.deepEqual([store.get().history?.filling, store.get().history?.before, store.get().history?.cursor], [false, 9, "77.3.history"]);
  assert.ok(!shown.includes("e11 e12"), `the page never held the part alone: ${JSON.stringify(shown)}`);
  // And it resumes from the snapshot that took its place.
  ws.close();
  store.wake();
  assert.match(Socket.all.at(-1)!.url, /after=77\.20/);
  off();
  store.dispose();
});

await test("W21-R02 something live before a window's rest means none is coming: what's before the part is a page away", async () => {
  const store = new SessionStore("history");
  const { ws, off } = openPart(store);
  assert.equal(store.get().history?.filling, true);
  const waiting = store.filled();
  // The rest comes before anything live. A live frame first: this socket brings none.
  ws.frame({ t: "events", events: [event(12)], rev: 11 });
  await waiting;
  await tick();
  assert.equal(store.get().history?.filling, false);
  assert.deepEqual(
    store.get().events.map((e) => e.id),
    ["e10", "e11", "e12"],
  );
  // A rest that comes after all isn't this window's any more; a page reads what's before the part.
  ws.frame({ t: "earlier", events: [event(7)], ...meta(7) });
  await tick();
  assert.equal(store.event("e7"), undefined);
  const fetch = pendingFetch();
  const page = store.loadOlder();
  assert.equal(fetch.count(), 1);
  fetch.finish({ events: [event(8), event(9)], history: meta(8) });
  await page;
  await tick();
  assert.deepEqual(
    store.get().events.map((e) => e.id),
    ["e8", "e9", "e10", "e11", "e12"],
  );
  off();
  store.dispose();
});

await test("W21-R02 a window that fits its first part is whole at once", async () => {
  const store = new SessionStore("history");
  const { off } = open(store);
  assert.equal(store.get().history?.filling, false);
  const fetch = pendingFetch();
  const p = store.loadOlder();
  assert.equal(fetch.count(), 1, "what's before it is asked for at once");
  fetch.finish({ events: [event(4)], history: meta(4) });
  await p;
  off();
  store.dispose();
});
