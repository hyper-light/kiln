/**
 * W21-R02: the agents' last words, for the rows that show them (src/api/last-words.ts). The list tells an agent
 * without them; a row reads them here, and what's counted is the requests that takes.
 */
import assert from "node:assert/strict";
import { test } from "node:test";

const sockets: Socket[] = [];
class Socket {
  static OPEN = 1;
  static CONNECTING = 0;
  static CLOSED = 3;
  readyState = Socket.OPEN;
  onopen: (() => void) | null = null;
  onmessage: ((m: { data: string }) => void) | null = null;
  onclose: (() => void) | null = null;
  constructor(readonly url = "") {
    sockets.push(this);
  }
  send() {}
  close() {}
}

/** What the server says each agent last said, the requests asked of it (each its ids), and whether it answers. */
const server: Record<string, string> = { a: "Done: a", b: "", c: "Done: c", d: "Done: d" };
const asked: string[][] = [];
let down = false;

Object.assign(globalThis, {
  WebSocket: Socket,
  location: { protocol: "http:", host: "localhost", pathname: "/agents", href: "http://localhost/agents" },
  document: Object.assign(new EventTarget(), { hidden: false }),
  window: Object.assign(new EventTarget(), {
    setTimeout: (fn: () => void, ms: number) => setTimeout(fn, ms).unref(),
    clearTimeout,
    setInterval: (fn: () => void, ms: number) => setInterval(fn, ms).unref(),
    clearInterval,
  }),
  fetch: async (url: string) => {
    const ids = new URL(String(url), "http://localhost").searchParams.get("summaries");
    if (ids === null) return new Response("{}");
    asked.push(ids.split(","));
    if (down) return new Response("no", { status: 503 });
    return new Response(JSON.stringify(Object.fromEntries(ids.split(",").flatMap((id) => (id in server ? [[id, server[id]]] : [])))));
  },
});

const { forgetLastWords, lastWords, lastWordsOf, listAsked, listedAgain, told } = await import("../src/api/last-words.ts");
const { events } = await import("../src/api/events.ts");
const { useAuth } = await import("../src/lib/auth.ts");

const tick = (ms = 0) => new Promise<void>((resolve) => setTimeout(resolve, ms));
const lean = (id: string) => ({ id });
const fresh = () => {
  forgetLastWords();
  asked.length = 0;
  down = false;
};

await test("W21-R02 an agent that says its last words is read from itself: nothing is asked", async () => {
  fresh();
  assert.equal(lastWords({ id: "a", summary: "Its own" }), "Its own");
  assert.equal(lastWords({ id: "b", summary: "" }), "", "one that has said nothing says so");
  await tick();
  assert.deepEqual(asked, []);
});

await test("W21-R02 the rows of one render ask in one request; until it answers they have no line, then theirs", async () => {
  fresh();
  assert.equal(lastWords(lean("a")), undefined);
  assert.equal(lastWords(lean("b")), undefined);
  assert.equal(lastWords(lean("gone")), undefined);
  assert.equal(lastWords(lean("a")), undefined, "asked again before the answer: the same request");
  await tick();
  assert.deepEqual(asked, [["a", "b", "gone"]]);
  assert.equal(lastWords(lean("a")), "Done: a");
  assert.equal(lastWords(lean("b")), "");
  assert.equal(lastWords(lean("gone")), "", "one the server says nothing of has no line to wait for");
  await tick();
  assert.equal(asked.length, 1, "known, they aren't asked for again");
});

await test("W21-R02 what the snapshot told needs no request, and a list's first rows wait only for what isn't known", async () => {
  fresh();
  told({ a: "Done: a", b: "" });
  assert.equal(lastWords(lean("a")), "Done: a");
  await lastWordsOf([lean("a"), lean("b"), { id: "x", summary: "Its own" }]);
  assert.deepEqual(asked, [], "all known or their own: nothing to wait for");
  await lastWordsOf([lean("a"), lean("c"), lean("d")]);
  assert.deepEqual(asked, [["c", "d"]]);
  assert.equal(lastWords(lean("c")), "Done: c");
});

await test("W21-R02 the list read again: what was told before it was asked for shows while it's asked again; what was told since stands", async () => {
  fresh();
  told({ a: "Done: a" });
  const since = listAsked();
  told({ c: "Done: c" });
  server.a = "Done: a, again";
  listedAgain(since);
  assert.equal(lastWords(lean("a")), "Done: a", "what it had shows meanwhile");
  assert.equal(lastWords(lean("c")), "Done: c");
  await tick();
  assert.deepEqual(asked, [["a"]], "only the one told before the list was asked for");
  assert.equal(lastWords(lean("a")), "Done: a, again");
  server.a = "Done: a";
});

await test("W21-R02 words that couldn't be read aren't asked for at every render: once the list is read again", async () => {
  fresh();
  down = true;
  assert.equal(lastWords(lean("a")), undefined);
  await tick();
  assert.equal(lastWords(lean("a")), undefined);
  await lastWordsOf([lean("a")]);
  await tick();
  assert.equal(asked.length, 1, "one request, however often its row draws");
  down = false;
  listedAgain(listAsked());
  assert.equal(lastWords(lean("a")), undefined);
  await tick();
  assert.equal(asked.length, 2);
  assert.equal(lastWords(lean("a")), "Done: a");
});

await test("W21-R02 an agent's news carries its last words: its row has them without asking, and one that closed has none kept", async () => {
  fresh();
  useAuth.setState({ kind: "authed", me: null }, true);
  events.start();
  await tick();
  const say = (ev: object) => sockets.at(-1)?.onmessage?.({ data: JSON.stringify(ev) });
  say({ type: "harness", info: { id: "n", summary: "Just said" } });
  assert.equal(lastWords(lean("n")), "Just said");
  await tick();
  assert.deepEqual(asked, []);
  say({ type: "harness_closed", id: "n" });
  assert.equal(lastWords(lean("n")), undefined);
  events.stop();
});
