import assert from "node:assert/strict";
import { test } from "node:test";

class Socket {
  static OPEN = 1;
  static CONNECTING = 0;
  static CLOSED = 3;
  readyState = Socket.CONNECTING;
  constructor(readonly url = "") {}
  send() {}
  close() {}
}

/** What the page asked of the server, and what the server answers next (in order). */
const asked: { method: string; url: string; body: unknown }[] = [];
const answers: (() => Response)[] = [];

Object.assign(globalThis, {
  WebSocket: Socket,
  location: { protocol: "http:", host: "localhost", pathname: "/", href: "http://localhost/" },
  document: Object.assign(new EventTarget(), { hidden: false }),
  window: Object.assign(new EventTarget(), {
    setTimeout: (fn: () => void, ms: number) => setTimeout(fn, ms).unref(),
    clearTimeout,
    setInterval: (fn: () => void, ms: number) => setInterval(fn, ms).unref(),
    clearInterval,
  }),
  fetch: async (url: string, init: RequestInit) => {
    asked.push({ method: init.method ?? "GET", url, body: init.body ? JSON.parse(String(init.body)) : undefined });
    const answer = answers.shift();
    return answer ? answer() : new Response("[]", { status: 200 });
  },
});

const { loaded, setData } = await import("../src/lib/resource.ts");
const { startAgents, startAgain, useStarting } = await import("../src/views/harness/starting.ts");
type Info = { id: string; status: string; name: string };

const tick = () => new Promise<void>((resolve) => setImmediate(resolve));
/** The lists of agents the pages read (lib/data.ts, use-agents.ts), as they are now. */
const LISTS = ["harness/sessions", "harness/agents"];
const listed = (key: string) => {
  let now: Info[] = [];
  setData<Info[]>(key, (l) => (now = l));
  return now.map((a) => `${a.id.slice(0, 3)}:${a.status}`);
};
const id = (c: string) => c.repeat(32);
const start = (ids: string[]) => startAgents({ ids, target: "agent:m1", spec: { kind: "claude", prompt: "Fix the tests" }, kindName: "Claude Code", machine: "m1", text: "Fix the tests", files: [] });
const ok = (infos: Partial<Info>[]) => () => new Response(JSON.stringify(infos.map((i) => ({ name: "Claude Code", status: "starting", ...i }))), { status: 200 });
const refused = (status: number, error: string) => () => new Response(JSON.stringify({ error }), { status });

for (const key of LISTS) await loaded(key, async () => []);

await test("W21-R15: an agent starts under the id made here, listed starting at once, and done once the server answers", async () => {
  answers.push(ok([{ id: id("a") }]));
  start([id("a")]);
  // Before any answer: its start under way, and it's listed, starting, on every list.
  assert.equal(useStarting.getState().starts[id("a")]?.state, "starting");
  for (const key of LISTS) assert.deepEqual(listed(key), ["aaa:starting"]);
  const ask = asked.at(-1);
  assert.equal(ask?.url, "/api/harness/sessions");
  assert.deepEqual((ask?.body as { ids?: string[] }).ids, [id("a")]);
  await tick();
  assert.equal(useStarting.getState().starts[id("a")]?.state, "started");
  for (const key of LISTS) assert.deepEqual(listed(key), ["aaa:starting"]);
});

/** The server's answer to a lookup of one agent: there (as it lists it), or not. */
const there = (i: string) => () => new Response(JSON.stringify({ id: i, name: "Claude Code", status: "running" }), { status: 200 });
const none = () => new Response(JSON.stringify({ error: "no such agent session" }), { status: 404 });
const settle = async () => {
  for (let i = 0; i < 5; i++) await tick();
};

await test("W21-R15: a start that's refused says why and leaves the lists; asked again, a 409 is a start only if the server has the agent", async () => {
  answers.push(refused(400, "machine m1 is not connected"), none);
  start([id("b")]);
  await settle();
  const failed = useStarting.getState().starts[id("b")];
  assert.equal(failed?.state, "failed");
  assert.equal(failed?.why, "machine m1 is not connected");
  for (const key of LISTS) assert.ok(!listed(key).includes("bbb:starting"));
  // Try again: listed again at once, under the same id; refused as there already, and it is: started.
  answers.push(refused(409, `an agent ${id("b")} is there already, or starting`), there(id("b")));
  startAgain(id("b"));
  assert.equal(useStarting.getState().starts[id("b")]?.state, "starting");
  for (const key of LISTS) assert.ok(listed(key).includes("bbb:starting"));
  assert.deepEqual((asked.at(-1)?.body as { ids?: string[] }).ids, [id("b")]);
  await settle();
  assert.equal(useStarting.getState().starts[id("b")]?.state, "started");
  assert.equal(asked.at(-1)?.url, `/api/harness/sessions/${id("b")}`, "the server was asked whether it's there");
  // A 409 for anything else (its workspace can't be there): not there, so a failure that says why.
  answers.push(refused(409, "an agent can't have a workspace of its own there: m1 hasn't said yet"), none);
  start([id("c")]);
  await settle();
  assert.equal(useStarting.getState().starts[id("c")]?.state, "failed");
  assert.match(useStarting.getState().starts[id("c")]?.why ?? "", /workspace of its own/);
});

await test("W21-R15: of several, those that started before another couldn't are listed; Try again asks only for the rest", async () => {
  answers.push(refused(400, "m1 already has 4 agents at work"), there(id("1")), none);
  start([id("1"), id("2")]);
  await settle();
  const partly = useStarting.getState().starts[id("1")];
  assert.equal(partly?.state, "failed");
  assert.deepEqual(
    partly?.started?.map((a) => a.id),
    [id("1")],
  );
  for (const key of LISTS) {
    assert.ok(listed(key).includes("111:running"), listed(key).join(" "));
    assert.ok(!listed(key).some((a) => a.startsWith("222")), listed(key).join(" "));
  }
  answers.push(ok([{ id: id("2") }]));
  startAgain(id("2"));
  const retry = asked.at(-1);
  assert.equal(retry?.url, "/api/harness/sessions");
  assert.deepEqual((retry?.body as { ids?: string[]; count?: number }).ids, [id("2")]);
  assert.equal((retry?.body as { count?: number }).count, 1);
  await settle();
  const all = useStarting.getState().starts[id("2")];
  assert.equal(all?.state, "started");
  assert.deepEqual(all?.started?.map((a) => a.id).sort(), [id("1"), id("2")].sort());
});

await test("W21-R15: from a server that makes its own ids, its agents take the place of those listed under the ids made here", async () => {
  answers.push(ok([{ id: id("e") }, { id: id("f") }]));
  start([id("d"), id("9")]);
  for (const key of LISTS) assert.ok(listed(key).includes("ddd:starting") && listed(key).includes("999:starting"));
  await tick();
  assert.deepEqual(
    useStarting.getState().starts[id("d")]?.started?.map((a) => a.id),
    [id("e"), id("f")],
  );
  for (const key of LISTS) {
    const now = listed(key);
    assert.ok(!now.includes("ddd:starting") && !now.includes("999:starting"), now.join(" "));
    assert.ok(now.includes("eee:starting") && now.includes("fff:starting"), now.join(" "));
  }
});

await test("W21-R15: what the server's events said of a starting agent stands over the start's own answer, which says less", async () => {
  answers.push(ok([{ id: id("7") }]));
  start([id("7")]);
  // Its machine's word first, as an event (in place of what was listed here).
  for (const key of LISTS) setData<Info[]>(key, (l) => l.map((a) => (a.id === id("7") ? { ...a, status: "running" } : a)));
  await tick();
  assert.equal(useStarting.getState().starts[id("7")]?.state, "started");
  for (const key of LISTS) assert.ok(listed(key).includes("777:running"), listed(key).join(" "));
});
