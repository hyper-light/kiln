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
  assert.match(ws.url, /recent_turns=8&event_bytes=262144/);
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
