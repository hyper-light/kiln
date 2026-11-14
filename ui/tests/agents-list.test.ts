import assert from "node:assert/strict";
import { test } from "node:test";

// The agents' list is read once as a page opens, for the page, Mission control and the shell's
// cards (views/harness/waiting.ts kept a list of its own, read as the shell mounted and again each
// time the event stream opened), and a burst of events is applied to it in one pass (W26-R15).

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

type Info = { id: string; name: string; status: string; approvals: { id: string; tool: string; tool_name: string; title: string; at: number }[]; last_activity: number; helper?: boolean };
const agent = (id: string, asks = 0): Info => ({
  id,
  name: `Agent ${id}`,
  status: "running",
  approvals: Array.from({ length: asks }, (_, i) => ({ id: `${id}-ask-${i}`, tool: "shell", tool_name: "Bash", title: "cargo test", at: 1 })),
  last_activity: 1,
});
const fleet: Info[] = [agent("a"), agent("b", 1), agent("c")];
let lists = 0;

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
    if (String(url).endsWith("/api/harness/sessions")) {
      lists++;
      return new Response(JSON.stringify(fleet));
    }
    return new Response("{}");
  },
});

const { loaded, resourceData } = await import("../src/lib/resource.ts");
const { harnessSessionsRes, agentsWith } = await import("../src/lib/data.ts");
const { useAuth } = await import("../src/lib/auth.ts");
const { events } = await import("../src/api/events.ts");
const { preloadAgents } = await import("../src/views/harness/use-agents.ts");
const { subscribeWaiting, getWaiting } = await import("../src/views/harness/waiting.ts");
type AgentInfo = import("../src/api/harness.ts").AgentInfo;
type ServerEvent = import("../src/api/types.ts").ServerEvent;

const tick = (ms = 0) => new Promise<void>((resolve) => setTimeout(resolve, ms));
const info = (i: Info) => i as unknown as AgentInfo;
const harness = (i: Info) => ({ type: "harness", info: i }) as unknown as ServerEvent;

await test("W26-R15: a burst of events is applied to the agents in one pass, each untouched agent its very entry", () => {
  const [a, b, c] = [info(agent("a")), info(agent("b")), info(agent("c"))];
  const list = [a, b, c];
  const b2 = agent("b", 2);
  const d = agent("d");
  const next = agentsWith(list, [harness(agent("b", 1)), harness(b2), harness(d), { type: "harness_closed", id: "c" } as unknown as ServerEvent]);
  assert.deepEqual(
    next?.map((x) => x.id),
    ["a", "b", "d"],
    "the newest word of each, new ones after, the closed one gone",
  );
  assert.equal(next?.[0], a, "an agent no event names keeps its entry");
  assert.equal(next?.[1], info(b2), "the last of its events stands");
  assert.equal(agentsWith(list, []), list, "no event: the list itself");
  assert.equal(agentsWith(list, [harness(a as unknown as Info)]), list, "an event that says what it has: the list itself");
  assert.deepEqual(
    agentsWith(list, [harness({ ...agent("a"), helper: true })])?.map((x) => x.id),
    ["b", "c"],
    "one no longer listed goes",
  );
  assert.equal(agentsWith(list, [{ type: "queue" } as unknown as ServerEvent]), undefined, "another kind of event can't be applied");
});

await test("W21-R02: the page, Mission control and the shell's cards read the agents once, and events keep all three", async () => {
  useAuth.setState({ kind: "authed", me: null }, true);
  events.start();
  let told = 0;
  const off = subscribeWaiting(() => {
    told++;
  });
  await loaded(harnessSessionsRes.key, harnessSessionsRes.fetch, harnessSessionsRes.opts);
  preloadAgents();
  await tick();
  assert.equal(lists, 1, "one read of the list for all of them");
  assert.deepEqual(
    getWaiting().map((a) => a.id),
    ["b"],
    "the shell's cards are the list's agents that ask",
  );
  assert.ok(told >= 1);
  // The stream opening (as it does a moment after the page) reads nothing again.
  const before = lists;
  sockets.at(-1)?.onopen?.();
  await tick();
  assert.equal(lists, before, "the stream's first opening doesn't read the list again");
  // News of an agent reaches the list and the cards from the one stream (gathered: lib/resource.ts's APPLY_MS).
  sockets.at(-1)?.onmessage?.({ data: JSON.stringify(harness(agent("c", 1))) });
  sockets.at(-1)?.onmessage?.({ data: JSON.stringify(harness(agent("b", 0))) });
  await tick(400);
  assert.deepEqual(
    getWaiting().map((a) => a.id),
    ["c"],
  );
  assert.deepEqual(
    resourceData(harnessSessionsRes)?.map((a) => `${a.id}:${a.approvals.length}`),
    ["a:0", "b:0", "c:1"],
  );
  assert.equal(lists, before, "and no read of the list for it");
  off();
  events.stop();
});
