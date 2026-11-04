import assert from "node:assert/strict";
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";
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
  frame(value: unknown) {
    this.onmessage?.({ data: JSON.stringify(value) });
  }
  said() {
    return this.sent.map((s) => JSON.parse(s) as Record<string, unknown>);
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
});
const { events } = await import("../src/api/events.ts");
const { watch, preload } = await import("../src/lib/resource.ts");
const tick = () => new Promise<void>((resolve) => setImmediate(resolve));
const wait = (ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms));
const last = () => Socket.all.at(-1)!;
const query = (s: Socket) => new URL(s.url).searchParams;
const emit = (ev: unknown) => (events as unknown as { handlers: Set<(e: unknown) => void> }).handlers.forEach((h) => h(ev));

// W21-R15: a page is sent only what it uses. events.ts's own (the queue's history) is always heard.
await test("the events asked of the server follow what's shown and what listens", async () => {
  assert.deepEqual(events.topics(), ["queue"]);
  const off = watch("topics/runs", async () => [], { on: ["run", "job"] }, () => {});
  assert.deepEqual(events.topics(), ["job", "queue", "run"], "a shown key's types");
  const offH = events.on(["harness", "harness_closed"], () => {});
  assert.deepEqual(events.topics(), ["harness", "harness_closed", "job", "queue", "run"]);
  await tick();
  off();
  assert.deepEqual(events.topics(), ["harness", "harness_closed", "queue"], "no longer shown: it loads again when it is, so it needs none");
  offH();
  assert.deepEqual(events.topics(), ["queue"]);
  // Loaded ahead of being shown: heard while it loads and after, until an event says it's out of date.
  preload("topics/ahead", async () => 1, { on: ["deploy"] });
  assert.ok(events.topics()?.includes("deploy"), "while it loads");
  await tick();
  assert.ok(events.topics()?.includes("deploy"), "loaded, not shown, not out of date");
  emit({ type: "deploy" });
  assert.deepEqual(events.topics(), ["queue"], "out of date: it loads again when shown");
  // A listener that names no types hears everything: the server is asked for all.
  const offAll = events.on(() => {});
  assert.equal(events.topics(), null);
  offAll();
});

await test("a listener hears only its types", () => {
  const heard: string[] = [];
  const off = events.on(["run"], (ev) => heard.push(ev.type));
  emit({ type: "job", job: { id: "j" } });
  emit({ type: "run", run: { id: "r" } });
  off();
  assert.deepEqual(heard, ["run"]);
});

await test("the server is told the topics: in its URL, then as they change, with where the feed was", async () => {
  events.start();
  try {
    const ws = last();
    const q = query(ws);
    assert.equal(q.get("batch"), "1");
    assert.equal(q.get("topics"), "queue");
    assert.equal(q.get("after"), null, "nothing to resume from yet");
    ws.onopen?.();
    ws.frame({ t: "batch", at: "5.10", events: [{ type: "queue", queued: 1, running: 0 }] });
    await tick();
    assert.deepEqual(ws.said(), [], "the topics it connected with are the server's");
    const off = events.on(["run"], () => {});
    await tick();
    assert.deepEqual(ws.said(), [{ t: "topics", topics: ["queue", "run"], since: "5.10" }]);
    off();
    await tick();
    assert.deepEqual(ws.said().at(-1), { t: "topics", topics: ["queue"] }, "fewer: no since (nothing to catch up on)");
    // Changes within one task are told once.
    const a = events.on(["job"], () => {});
    a();
    const b = events.on(["job"], () => {});
    await tick();
    assert.equal(ws.sent.length, 3);
    b();
    await tick();
  } finally {
    events.stop();
  }
});

await test("a server that doesn't batch is never sent topics", async () => {
  events.start();
  try {
    const ws = last();
    ws.onopen?.();
    ws.frame({ type: "queue", queued: 0, running: 0 });
    const off = events.on(["run"], () => {});
    await tick();
    off();
    assert.deepEqual(ws.said(), []);
  } finally {
    events.stop();
  }
});

await test("a reconnect resumes and loads nothing again; one that can't loads once", async () => {
  let reads = 0;
  let reopened = 0;
  const offReopen = events.onReopen(() => reopened++);
  const off = watch(
    "topics/shown",
    async () => {
      reads++;
      return reads;
    },
    { on: ["run"] },
    () => {},
  );
  events.start();
  try {
    let ws = last();
    assert.equal(query(ws).get("topics"), "queue,run");
    ws.onopen?.();
    ws.frame({ t: "batch", at: "5.10", events: [] });
    // (This stream was open in the tests before: this open is a reconnect that couldn't resume.)
    await wait(20);
    const base = reads;
    const back = reopened;
    // Lost, and back: it asks for what came after where it was.
    ws.close();
    events.retryNow();
    ws = last();
    assert.equal(query(ws).get("after"), "5.10");
    ws.onopen?.();
    ws.frame({ t: "batch", at: "5.12", events: [{ type: "updates" }] });
    await wait(50);
    assert.equal(reads, base, "resumed: nothing loaded again");
    assert.equal(reopened, back + 1, "a reconnect is still heard (a new build is looked for)");
    // The server restarted: its first batch says resync. What's shown loads again, once.
    ws.close();
    events.retryNow();
    ws = last();
    assert.equal(query(ws).get("after"), "5.12");
    ws.onopen?.();
    ws.frame({ t: "batch", at: "6.1", events: [{ type: "resync" }] });
    await tick();
    assert.equal(reads, base + 1);
    await wait(1100);
    assert.equal(reads, base + 1, "once, not once more for the resync");
    // An older server: no batch, no resume.
    ws.close();
    events.retryNow();
    ws = last();
    ws.onopen?.();
    ws.frame({ type: "queue", queued: 0, running: 0 });
    await wait(20);
    assert.equal(reads, base + 2);
    assert.equal(reopened, back + 3);
    // A resync later on (it fell behind) loads what's shown again, as before.
    ws.frame({ type: "resync" });
    await wait(1300);
    assert.equal(reads, base + 3);
  } finally {
    off();
    offReopen();
    events.stop();
  }
});

await test("a batch's events are each read as a frame: one with an unknown tag is told, the rest applied", async () => {
  const heard: string[] = [];
  const off = events.on((ev) => heard.push(ev.type));
  events.start();
  try {
    const ws = last();
    ws.onopen?.();
    ws.frame({ t: "batch", at: "1.2", events: [{ type: "terminals" }, { type: "hologram" }, { type: "updates" }] });
    assert.deepEqual(heard, ["terminals", "updates"]);
    assert.deepEqual(ws.said(), [{ t: "unread", of: "hologram", why: "unknown frame" }]);
  } finally {
    off();
    events.stop();
  }
});

/** Every call that listens to the events socket in `text`: what it listens to, and the tags its handler compares with. */
function listeners(text: string): { at: number; types: string[] | "function" | "all"; compared: string[] }[] {
  const out: { at: number; types: string[] | "function" | "all"; compared: string[] }[] = [];
  const call = /\b(?:events\.on|useServerEvents)\(/g;
  for (let m = call.exec(text); m; m = call.exec(text)) {
    const lineStart = text.lastIndexOf("\n", m.index) + 1;
    const line = text.slice(lineStart, m.index).trim();
    if (line.startsWith("*") || line.startsWith("//")) continue;
    let i = m.index + m[0].length;
    let depth = 1;
    const open = i;
    while (i < text.length && depth > 0) {
      const c = text[i];
      if (c === "(") depth++;
      else if (c === ")") depth--;
      i++;
    }
    const args = text.slice(open, i - 1).trimStart();
    let types: string[] | "function" | "all" = "all";
    if (args.startsWith("[")) types = [...args.slice(0, args.indexOf("]")).matchAll(/"([^"]+)"/g)].map((x) => x[1]!);
    else if (/^[A-Za-z_$][\w$]*\s*,/.test(args)) types = "function";
    const compared = [...args.matchAll(/\bev\.type\s*[!=]==?\s*"([^"]+)"/g)].map((x) => x[1]!);
    out.push({ at: lineStart, types, compared });
  }
  return out;
}

function sources(dir: string): string[] {
  return readdirSync(dir).flatMap((n) => {
    const p = join(dir, n);
    return statSync(p).isDirectory() ? sources(p) : /\.tsx?$/.test(n) ? [p] : [];
  });
}

/** What's wrong with the listeners of `text`: one that hears everything, or compares with a tag it doesn't ask for. */
function wrong(file: string, text: string): string[] {
  return listeners(text).flatMap((l) => {
    const where = `${file}:${text.slice(0, l.at).split("\n").length}`;
    if (l.types === "all") return [`${where} listens to every event: name its types`];
    if (l.types === "function") return [];
    const types = l.types;
    return l.compared.filter((t) => !types.includes(t)).map((t) => `${where} handles "${t}", which it doesn't ask for`);
  });
}

await test("every listener of the events socket names its types, and handles none it doesn't ask for", () => {
  // The check itself catches one that would silently lose an event.
  assert.deepEqual(wrong("x.ts", `useServerEvents(["run"], (ev) => {\n  if (ev.type === "job") reload();\n});`), ['x.ts:1 handles "job", which it doesn't ask for']);
  assert.deepEqual(wrong("x.ts", `  events.on((ev) => {});`), ["x.ts:1 listens to every event: name its types"]);
  const src = join(import.meta.dirname, "../src");
  const found: string[] = [];
  let calls = 0;
  for (const file of sources(src)) {
    const text = readFileSync(file, "utf8");
    calls += listeners(text).length;
    found.push(...wrong(file.slice(src.length + 1), text));
  }
  assert.deepEqual(found, []);
  assert.ok(calls >= 19, `found ${calls} listeners: the scan misses some`);
});
