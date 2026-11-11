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
  sent: string[] = [];
  constructor(url?: string) {
    this.url = url ?? "";
    Socket.all.push(this);
  }
  send(value: string) {
    this.sent.push(value);
  }
  close() {
    this.readyState = Socket.CLOSED;
    this.onclose?.();
  }
  frame(value: unknown) {
    this.onmessage?.({ data: JSON.stringify(value) });
  }
  binary(text: string) {
    this.onmessage?.({ data: new TextEncoder().encode(text).buffer });
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
const tick = () => new Promise<void>((resolve) => setImmediate(resolve));
const event = (seq: number) => ({ id: `e${seq}`, seq, at: seq, kind: "assistant", text: "text" });
const state = { status: "running", tasks: [], delivery: {}, dropped: 0 };
const snapshot = (id: string, events: unknown[]) => [{ t: "open", info: { id, status: "running" } }, { t: "state", state }, { t: "events", events }, { t: "synced" }];

await test("W21-R02 an indexed event reader exposes published frames and preserves unrelated identities", async () => {
  const store = new SessionStore("published-event");
  const off = store.subscribe(() => {});
  const ws = Socket.all.at(-1)!;
  try {
    for (const frame of snapshot("published-event", [event(1), event(2), event(3)])) ws.frame(frame);
    await tick();
    const read = store.event;
    const first = read("e1");
    const streamed = read("e2");
    const untouched = read("e3");
    assert.equal(read("missing"), undefined);
    ws.frame({ t: "stream", id: "e2", text: " appended" });
    ws.frame({ t: "events", events: [{ ...event(1), text: "replaced" }, event(4)] });
    assert.equal(read("e1"), first, "a replacement is private until its frame publishes");
    assert.equal(read("e2"), streamed, "stream text cannot leak out of the published snapshot");
    assert.equal(read("e4"), undefined, "an appended event is not yet published");
    await tick();
    assert.equal(store.event, read, "consumers keep one stable getter");
    const replaced = read("e1");
    const appended = read("e2");
    assert.equal(replaced?.kind === "assistant" && replaced.text, "replaced");
    assert.equal(appended?.kind === "assistant" && appended.text, "text appended");
    assert.equal(read("e3"), untouched, "unrelated targets retain identity for memoized subscribers");
    assert.equal(read("e4"), store.get().events.at(-1));
    // An out-of-order append rebuilds positions without changing the other published objects.
    ws.frame({ t: "events", events: [{ ...event(0), id: "earlier" }] });
    await tick();
    assert.equal(store.get().events[0]?.id, "earlier");
    assert.equal(read("e3"), untouched);
    assert.equal(read("e1"), replaced);
    assert.equal(read("earlier"), store.get().events[0]);
    store.dispose();
    assert.equal(read("e1"), undefined, "retired authentication/cache epochs expose no old event");
  } finally {
    off();
  }
});

await test("live history stays bounded and retained events still update in place", async () => {
  const store = new SessionStore("history");
  const off = store.subscribe(() => {});
  const ws = Socket.all.at(-1)!;
  try {
    for (const frame of snapshot(
      "history",
      Array.from({ length: 4000 }, (_, i) => event(i)),
    ))
      ws.frame(frame);
    await tick();
    ws.frame({ t: "events", events: Array.from({ length: 4000 }, (_, i) => event(i + 4000)) });
    ws.frame({ t: "state", state: { ...state, dropped: 4000 } });
    await tick();
    assert.equal(store.get().events.length, 4000);
    assert.equal(store.get().events[0]?.id, "e4000");
    assert.equal(store.event("e0"), undefined, "W21-R02 trimmed quote targets leave the published index");
    assert.equal(store.event("e4000"), store.get().events[0]);
    ws.frame({ t: "stream", id: "e7999", text: " appended" });
    ws.frame({ t: "events", events: [{ ...event(4500), text: "replaced" }] });
    await tick();
    assert.equal(store.get().events.find((e) => e.id === "e4500")?.kind === "assistant" && store.get().events.find((e) => e.id === "e4500")?.text, "replaced");
    assert.equal(store.get().events.at(-1)?.kind === "assistant" && store.get().events.at(-1)?.text, "text appended");
    assert.equal(new Set(store.get().events.map((e) => e.id)).size, 4000);
    assert.equal(
      store.event("e4500"),
      store.get().events.find((e) => e.id === "e4500"),
    );
    assert.equal(store.event("e7999"), store.get().events.at(-1));
  } finally {
    off();
  }
});

await test("a previous connection's unpack cannot let text overtake a new snapshot", async () => {
  const original = globalThis.DecompressionStream;
  const release: (() => void)[] = [];
  // Control completion of unpacking without depending on compression timing.
  globalThis.DecompressionStream = class {
    constructor() {
      const wait = new Promise<void>((resolve) => release.push(resolve));
      return new TransformStream({
        async transform(chunk, controller) {
          await wait;
          controller.enqueue(chunk);
        },
      });
    }
  } as typeof DecompressionStream;
  const store = new SessionStore("reconnect");
  const off = store.subscribe(() => {});
  const packed = snapshot("reconnect", [event(1)])
    .map((f) => JSON.stringify(f))
    .join("\n");
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
    assert.equal(store.event("e4"), store.get().events.at(-1), "W21-R02 resumed positions follow the published snapshot");
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
    assert.equal(store.event("e1"), undefined);
    assert.equal(store.event("e9"), store.get().events[0]);
  } finally {
    off();
  }
});

await test("cached resources do not process live events while nothing shows them", async () => {
  const { preload } = await import("../src/lib/resource.ts");
  const { events } = await import("../src/api/events.ts");
  let reads = 0;
  let applied = 0;
  preload(
    "test/idle",
    async () => {
      reads++;
      return 0;
    },
    {
      on: ["queue"],
      apply: (value) => {
        applied++;
        return value + 1;
      },
    },
  );
  await tick();
  events.start();
  try {
    const ws = Socket.all.at(-1)!;
    ws.frame({ type: "queue", queued: 1, running: 1 });
    ws.frame({ type: "resync" });
    await new Promise<void>((resolve) => setTimeout(resolve, 300));
    assert.equal(reads, 1);
    assert.equal(applied, 0);
  } finally {
    events.stop();
  }
});

await test("what of its messages was refused comes with its events, on the socket it has: no new snapshot, no reload", async () => {
  const { events } = await import("../src/api/events.ts");
  const store = new SessionStore("refused");
  const off = store.subscribe(() => {});
  const ws = Socket.all.at(-1)!;
  events.start();
  const feed = Socket.all.at(-1)!;
  const refused = (n: number) => ({
    counts: [{ wire: "runner_payload", code: "unknown", n }],
    newest: [{ at: 1, unread: { wire: "runner_payload", from: { runner: { name: "box", instance: "main" } }, what: "update kind teleport", code: "unknown", why: "no such update" } }],
  });
  try {
    for (const frame of snapshot("refused", [event(1)])) ws.frame(frame);
    await tick();
    assert.equal(store.get().info?.refused, undefined);
    feed.frame({ type: "harness", info: { id: "refused", status: "running", refused: refused(1) } });
    assert.deepEqual(store.get().info?.refused, refused(1));
    // Another agent's says nothing of this one's.
    feed.frame({ type: "harness", info: { id: "other", status: "running", refused: refused(5) } });
    assert.deepEqual(store.get().info?.refused, refused(1));
    feed.frame({ type: "harness", info: { id: "refused", status: "running", refused: refused(2) } });
    assert.equal(store.get().info?.refused?.counts[0]?.n, 2);
    assert.equal(store.get().info?.status, "running", "the rest of what it shows stays");
    assert.equal(Socket.all.at(-1), feed, "the session's socket stays: nothing is read again");
  } finally {
    off();
    events.stop();
  }
});

await test("a message's id is a UUIDv7 made once: 32 hex digits, its time first, and never another's (CP-T3.2)", async () => {
  const { clientId } = await import("../src/api/harness.ts");
  const ids = Array.from({ length: 10_000 }, () => clientId());
  for (const id of ids.slice(0, 100)) assert.match(id, /^[0-9a-f]{12}7[0-9a-f]{3}[89ab][0-9a-f]{15}$/);
  assert.equal(new Set(ids).size, ids.length);
  const ms = Number.parseInt(ids[0]!.slice(0, 12), 16);
  assert.ok(Math.abs(ms - Date.now()) < 60_000, `its time: ${ms}`);
  assert.ok(
    ids.every((id, i) => i === 0 || id.slice(0, 12) >= ids[i - 1]!.slice(0, 12)),
    "made later, sorted later",
  );
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
  } finally {
    off();
  }
});

await test("W21-R02: data-only prefetch closes at a complete snapshot, sends no commands, and expires without reviving", async () => {
  const store = new SessionStore("spec-complete");
  const lease = store.prefetch(4096)!;
  const ws = Socket.all.at(-1)!;
  for (const frame of snapshot(store.id, [event(1)])) ws.frame(frame);
  assert.equal(await lease.ready, true);
  assert.equal(store.get().ready, true);
  assert.equal(store.get().conn, "reconnecting", "cached history does not advertise a closed command channel as live");
  assert.deepEqual(
    store.get().events.map((e) => e.id),
    ["e1"],
  );
  assert.equal(ws.readyState, Socket.CLOSED);
  assert.deepEqual(ws.sent, []);
  lease.cancel();
  ws.frame({ t: "events", events: [event(2)] });
  store.wake();
  assert.equal(store.get().ready, false);
  assert.deepEqual(store.get().events, []);
  assert.equal(store.send({ type: "interrupt" }), false);
});

await test("W21-R02: acquiring a prefetched store promotes before a child can suspend; late hover cancellation cannot close it", async () => {
  const { prefetchSession, sessionStore } = await import("../src/api/harness.ts");
  const lease = prefetchSession("spec-promote", 128)!;
  const ws = Socket.all.at(-1)!;
  const count = Socket.all.length;
  // Acquisition precedes ready()/subscribe() in AgentConsole, which can suspend in between.
  const store = sessionStore("spec-promote");
  lease.cancel();
  assert.equal(await lease.ready, false);
  assert.equal(ws.readyState, Socket.OPEN);
  assert.equal(Socket.all.length, count);
  const off = store.subscribe(() => {});
  try {
    for (const frame of snapshot(store.id, [{ ...event(1), text: "x".repeat(1024) }])) ws.frame(frame);
    await store.ready();
    assert.equal(store.get().events.length, 1, "foreground work is no longer limited by the speculative budget");
    assert.equal(Socket.all.length, count);
    assert.equal(store.send({ type: "interrupt" }), true);
    assert.equal(ws.sent.filter((line) => JSON.parse(line).t === "cmd").length, 1);
  } finally {
    off();
    store.dispose();
  }
});

await test("W21-R02: a completed prefetched snapshot promotes into normal revision resume, with no history truncation", async () => {
  const { prefetchSession, sessionStore } = await import("../src/api/harness.ts");
  const lease = prefetchSession("spec-resume", 4096)!;
  const ws = Socket.all.at(-1)!;
  ws.frame({ t: "open", info: { id: "spec-resume", status: "running" }, born: 77, rev: 10 });
  for (const frame of snapshot("spec-resume", [event(1)]).slice(1)) ws.frame(frame);
  assert.equal(await lease.ready, true);
  const store = sessionStore("spec-resume");
  lease.cancel();
  const off = store.subscribe(() => {});
  try {
    const resumed = Socket.all.at(-1)!;
    assert.notEqual(resumed, ws);
    resumed.readyState = Socket.CONNECTING;
    assert.equal(store.get().conn, "reconnecting");
    assert.equal(store.send({ type: "interrupt" }), false, "cached ready data cannot send through a connecting socket");
    assert.match(resumed.url, /[?&]after=77\.10(&|$)/);
    assert.deepEqual(
      store.get().events.map((e) => e.id),
      ["e1"],
    );
    resumed.readyState = Socket.OPEN;
    resumed.frame({ t: "resumed", info: { id: store.id, status: "running" }, born: 77, rev: 11 });
    assert.equal(store.get().conn, "open", "the real open frame proves that the command channel is available");
    resumed.frame({ t: "events", events: [event(2)] });
    resumed.frame({ t: "synced" });
    await tick();
    assert.deepEqual(
      store.get().events.map((e) => e.id),
      ["e1", "e2"],
    );
  } finally {
    off();
    store.dispose();
  }
});

await test("W21-R02: speculative text uses a UTF-8 byte budget and rejects before parsing oversized frames", async () => {
  const store = new SessionStore("spec-text-limit");
  const lease = store.prefetch(100)!;
  const ws = Socket.all.at(-1)!;
  ws.onmessage?.({ data: "é".repeat(60) }); // 60 UTF-16 code units but 120 UTF-8 bytes.
  assert.equal(await lease.ready, false);
  assert.equal(ws.readyState, Socket.CLOSED);
  assert.equal(store.get().ready, false);
  assert.deepEqual(ws.sent, [], "oversized speculative input is not parsed or answered");
});

await test("W21-R02: speculative gzip expansion is stopped while decoding, before parsing an expanded snapshot", async () => {
  const original = globalThis.DecompressionStream;
  let cancelled = false;
  globalThis.DecompressionStream = class {
    readable = new ReadableStream({
      pull(controller) {
        controller.enqueue(new Uint8Array(65));
      },
      cancel() {
        cancelled = true;
      },
    });
    writable = new WritableStream();
  } as typeof DecompressionStream;
  const store = new SessionStore("spec-gzip-limit");
  try {
    const lease = store.prefetch(128)!;
    const ws = Socket.all.at(-1)!;
    ws.binary("small compressed input");
    assert.equal(await lease.ready, false);
    await tick();
    assert.equal(cancelled, true);
    assert.equal(ws.readyState, Socket.CLOSED);
    assert.equal(store.get().ready, false);
    assert.deepEqual(ws.sent, []);
  } finally {
    store.dispose();
    globalThis.DecompressionStream = original;
  }
});

await test("W21-R02: queued packed frames are bounded even while the first decoder is blocked", async () => {
  const original = globalThis.DecompressionStream;
  let release = () => {};
  const wait = new Promise<void>((resolve) => {
    release = resolve;
  });
  globalThis.DecompressionStream = class {
    constructor() {
      return new TransformStream({
        async transform(chunk, controller) {
          await wait;
          controller.enqueue(chunk);
        },
      });
    }
  } as typeof DecompressionStream;
  const store = new SessionStore("spec-packed-queue");
  try {
    const lease = store.prefetch(128)!;
    const ws = Socket.all.at(-1)!;
    ws.binary("a".repeat(50));
    await tick();
    ws.binary("b".repeat(50));
    ws.binary("c".repeat(50));
    assert.equal(await lease.ready, false);
    assert.equal(ws.readyState, Socket.CLOSED);
    assert.equal(store.get().ready, false);
  } finally {
    release();
    store.dispose();
    globalThis.DecompressionStream = original;
    await tick();
  }
});

await test("W21-R02: auth loss clears private stores and commands, and late packed frames or wakeups cannot restore them", async () => {
  const { prefetchSession, sessionStore } = await import("../src/api/harness.ts");
  const { setAuthed } = await import("../src/api/client.ts");
  const original = globalThis.DecompressionStream;
  let release = () => {};
  const wait = new Promise<void>((resolve) => {
    release = resolve;
  });
  globalThis.DecompressionStream = class {
    constructor() {
      return new TransformStream({
        async transform(chunk, controller) {
          await wait;
          controller.enqueue(chunk);
        },
      });
    }
  } as typeof DecompressionStream;
  setAuthed(true);
  const lease = prefetchSession("spec-auth", 4096)!;
  const ws = Socket.all.at(-1)!;
  ws.binary(
    snapshot("spec-auth", [event(1)])
      .map((f) => JSON.stringify(f))
      .join("\n"),
  );
  await tick();
  const active = sessionStore("spec-active-auth");
  const off = active.subscribe(() => {});
  active.send({ type: "interrupt" });
  const sockets = Socket.all.length;
  try {
    setAuthed(false);
    assert.equal(await lease.ready, false);
    release();
    await tick();
    await tick();
    const old = Socket.all.find((s) => s.url.includes("spec-auth"))!;
    old.frame({ t: "synced" });
    active.wake();
    assert.equal(active.send({ type: "interrupt" }), false);
    assert.equal(active.get().ready, false);
    assert.deepEqual(active.get().asked, []);
    assert.equal(Socket.all.length, sockets);
    setAuthed(true);
    assert.notEqual(sessionStore("spec-active-auth"), active, "another login gets a new store");
  } finally {
    release();
    off();
    active.dispose();
    globalThis.DecompressionStream = original;
    setAuthed(false);
  }
});

await test("events about a session don't connect a socket that just died again at once, each its own snapshot", async () => {
  const { events } = await import("../src/api/events.ts");
  // The event stream's own listeners, as a message on it calls them.
  const handlers = (events as unknown as { handlers: Set<(ev: unknown) => void> }).handlers;
  const store = new SessionStore("busy");
  const off = store.subscribe(() => {});
  try {
    // It dies before its snapshot is in: it goes again after its backoff, not at the next event.
    Socket.all.at(-1)!.close();
    const sockets = Socket.all.length;
    for (let i = 0; i < 20; i++) handlers.forEach((h) => h({ type: "harness", info: { id: "busy", status: "running" } }));
    await tick();
    assert.equal(Socket.all.length, sockets, "a turn's events connected it again");
    // Asked (the tab shown again): at once all the same.
    store.wake();
    assert.equal(Socket.all.length, sockets + 1);
  } finally {
    off();
  }
});

await test("an event about a session taken as gone connects it at once: its machine came back", async () => {
  const { events } = await import("../src/api/events.ts");
  const handlers = (events as unknown as { handlers: Set<(ev: unknown) => void> }).handlers;
  const store = new SessionStore("back");
  const off = store.subscribe(() => {});
  try {
    const ws = Socket.all.at(-1)!;
    for (const frame of snapshot("back", [event(1)])) ws.frame(frame);
    await tick();
    // The server says it's gone (its machine went away): it's looked for only now and then.
    ws.frame({ t: "closed" });
    ws.close();
    await tick();
    const sockets = Socket.all.length;
    // An event says it's there again: it connects at once, not at the next look.
    handlers.forEach((h) => h({ type: "harness", info: { id: "back", status: "running" } }));
    await tick();
    assert.equal(Socket.all.length, sockets + 1, "the event connected it again");
  } finally {
    off();
  }
});

const boundedView = (queueRevision: string, revision = queueRevision) => ({
  revision,
  revisions: { queue: queueRevision },
  deferred: { queue: { kind: "array", len: 564 } },
  counts: { queue: 564, approvals: 0, tasks: 0, tasks_running: 0, todos: 0, todos_done: 0 },
});

await test("W20: bounded status recovers on an open socket while the queue stays deferred", async () => {
  const store = new SessionStore("bounded-status");
  const off = store.subscribe(() => {});
  const ws = Socket.all.at(-1)!;
  try {
    ws.frame({ t: "open", info: { id: store.id, status: "disconnected" } });
    ws.frame({ t: "state", state: { ...state, status: "disconnected", queue: [] } });
    ws.frame({ t: "synced" });
    await tick();
    ws.frame({ t: "state", state: { ...state, status: "idle", queue: [], view: boundedView("r1") } });
    await tick();
    assert.equal(store.get().state?.status, "idle");
    assert.equal(store.get().info?.status, "idle");
    assert.equal(store.get().conn, "open");
    const read = store.readState("queue");
    const command = JSON.parse(ws.sent.at(-1)!);
    assert.equal(command.c.type, "state_read");
    assert.equal(command.c.read.revision, "r1");
    ws.frame({
      t: "reply",
      client: command.c.client,
      value: { result: { type: "page", revision: "r1", entries: [{ key: { type: "index", index: 0 }, value: { id: "q0", text: "complete", at: 1 } }], next: 1 } },
    });
    await read;
    assert.equal(store.get().statePages.queue?.phase, "ready");
    assert.deepEqual(store.get().state?.queue, [], "a page never accumulates into the control state");
  } finally {
    store.dispose();
    off();
  }
});

await test("W20: one field page replaces its predecessor and ignores superseded reads", async () => {
  const store = new SessionStore("bounded-page");
  const off = store.subscribe(() => {});
  const ws = Socket.all.at(-1)!;
  try {
    for (const frame of snapshot(store.id, [])) ws.frame(frame);
    ws.frame({ t: "state", state: { ...state, queue: [], view: boundedView("r1") } });
    await tick();
    const first = store.readState("queue");
    const firstCommand = JSON.parse(ws.sent.at(-1)!).c;
    const second = store.readState("queue", [], 40);
    const secondCommand = JSON.parse(ws.sent.at(-1)!).c;
    ws.frame({
      t: "reply",
      client: secondCommand.client,
      value: { result: { type: "page", revision: "r1", entries: [{ key: { type: "index", index: 40 }, value: { id: "q40", text: "later", at: 1 } }], next: 41 } },
    });
    await second;
    ws.frame({
      t: "reply",
      client: firstCommand.client,
      value: { result: { type: "page", revision: "r1", entries: [{ key: { type: "index", index: 0 }, value: { id: "q0", text: "earlier", at: 1 } }] } },
    });
    await first;
    assert.equal(store.get().statePages.queue?.read.offset, 40);
    assert.equal(Object.keys(store.get().statePages).length, 1);
    const third = store.readState("queue", [], 41);
    const thirdCommand = JSON.parse(ws.sent.at(-1)!).c;
    ws.frame({ t: "state", state: { ...state, queue: [], view: boundedView("r2") } });
    await tick();
    ws.frame({ t: "reply", client: thirdCommand.client, value: { result: { type: "page", revision: "r1", entries: [] } } });
    await third;
    assert.deepEqual(store.get().statePages, {}, "a stale page cannot join a newer projection");
  } finally {
    store.dispose();
    off();
  }
});

await test("W20: paged queue edits carry a revision and stale refusals stay in place", async () => {
  const store = new SessionStore("bounded-edit");
  const off = store.subscribe(() => {});
  const ws = Socket.all.at(-1)!;
  try {
    for (const frame of snapshot(store.id, [])) ws.frame(frame);
    ws.frame({ t: "state", state: { ...state, queue: [], view: boundedView("r1") } });
    await tick();
    const read = store.readState("queue");
    const command = JSON.parse(ws.sent.at(-1)!).c;
    ws.frame({ t: "reply", client: command.client, value: { result: { type: "page", revision: "r1", entries: [{ key: { type: "index", index: 0 }, value: { id: "q", text: "full", at: 1 } }] } } });
    await read;
    const edit = store.changeState("queue", { type: "queue_edit", id: "q", text: "changed" });
    const mutation = JSON.parse(ws.sent.at(-1)!).c;
    assert.equal(mutation.type, "at_state");
    assert.equal(mutation.revision, "r1");
    assert.deepEqual(mutation.command, { type: "queue_edit", id: "q", text: "changed" });
    ws.frame({ t: "reply", client: mutation.client, value: { error: { code: "stale" } } });
    await edit;
    assert.equal(store.get().statePages.queue?.phase, "error");
    assert.match(store.get().statePages.queue?.error ?? "", /changed/);
    const pending = store.readState("queue");
    store.dispose();
    await pending;
    assert.deepEqual(store.get().statePages, {}, "disposal releases pages and pending readers");
  } finally {
    store.dispose();
    off();
  }
});

await test("W20: unrelated status changes preserve a queue page and its in-flight read", async () => {
  const store = new SessionStore("field-revision");
  const off = store.subscribe(() => {});
  const ws = Socket.all.at(-1)!;
  try {
    for (const frame of snapshot(store.id, [])) ws.frame(frame);
    ws.frame({ t: "state", state: { ...state, queue: [], view: boundedView("queue-1", "global-1") } });
    await tick();
    const first = store.readState("queue");
    const command = JSON.parse(ws.sent.at(-1)!).c;
    ws.frame({ t: "state", state: { ...state, status: "idle", queue: [], usage: { output_tokens: 20 }, view: boundedView("queue-1", "global-2") } });
    await tick();
    ws.frame({
      t: "reply",
      client: command.client,
      value: { result: { type: "page", revision: "queue-1", entries: [{ key: { type: "index", index: 0 }, value: { id: "q", text: "unchanged", at: 1 } }] } },
    });
    await first;
    const held = store.get().statePages.queue;
    assert.equal(held?.phase, "ready");
    ws.frame({ t: "state", state: { ...state, status: "running", queue: [], view: boundedView("queue-1", "global-3") } });
    await tick();
    assert.equal(store.get().statePages.queue, held);
    ws.frame({ t: "state", state: { ...state, queue: [], view: boundedView("queue-2", "global-4") } });
    await tick();
    assert.equal(store.get().statePages.queue, undefined, "changing queue contents invalidates only its field page");
  } finally {
    store.dispose();
    off();
  }
});

await test("W20: bounded queue metadata acknowledges listed messages without losing unseen ones", async () => {
  const store = new SessionStore("partial-index");
  const off = store.subscribe(() => {});
  const ws = Socket.all.at(-1)!;
  try {
    for (const frame of snapshot(store.id, [])) ws.frame(frame);
    await tick();
    store.send({ type: "send", id: "listed", text: "one", mode: "queue", attachments: [] });
    store.send({ type: "send", id: "not-listed", text: "two", mode: "queue", attachments: [] });
    ws.frame({ t: "state", state: { ...state, queue: [], view: { ...boundedView("queue-1"), queue_index: { entries: [{ id: "listed", digest: "abc", len: 3 }], complete: false, next: 1 } } } });
    await tick();
    assert.deepEqual(
      store.get().outgoing.map((m) => m.id),
      ["not-listed"],
    );
    ws.frame({ t: "state", state: { ...state, queue: [], view: { ...boundedView("queue-1"), queue_index: { entries: [], complete: false, next: 0 } } } });
    await tick();
    assert.deepEqual(
      store.get().outgoing.map((m) => m.id),
      ["not-listed"],
      "an incomplete index's missing id does not drop or fail a message",
    );
    const read = store.readState("$queue_index", [{ type: "id", id: "not-listed" }]);
    const command = JSON.parse(ws.sent.at(-1)!).c;
    assert.deepEqual(command.read.path, [{ type: "id", id: "not-listed" }]);
    assert.equal(command.read.revision, "queue-1");
    ws.frame({ t: "reply", client: command.client, value: { result: { type: "value", revision: "queue-1", value: { id: "not-listed", digest: "def", len: 3 } } } });
    await read;
    assert.deepEqual(store.get().outgoing, [], "an exact index lookup is positive delivery evidence");
  } finally {
    store.dispose();
    off();
  }
});

await test("W20: a complete small state preserves ordinary queue data and command behavior", async () => {
  const store = new SessionStore("small-state");
  const off = store.subscribe(() => {});
  const ws = Socket.all.at(-1)!;
  try {
    for (const frame of snapshot(store.id, [])) ws.frame(frame);
    const queue = [{ id: "small", text: "complete text", at: 1 }];
    const view = { ...boundedView("queue-1"), deferred: {}, counts: { queue: 1, approvals: 0, tasks: 0, tasks_running: 0, todos: 0, todos_done: 0 } };
    ws.frame({ t: "state", state: { ...state, queue, approvals: [], view } });
    await tick();
    assert.deepEqual(store.get().state?.queue, queue);
    assert.deepEqual(store.get().statePages, {});
    const before = ws.sent.length;
    await store.readState("queue");
    assert.equal(ws.sent.length, before, "ordinary inline data needs no paging request");
    store.send({ type: "queue_edit", id: "small", text: "edit" });
    assert.equal(JSON.parse(ws.sent.at(-1)!).c.type, "queue_edit", "ordinary typed actions retain their existing protocol");
  } finally {
    store.dispose();
    off();
  }
});
