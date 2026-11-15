/**
 * W21-R02: the agents, lean, down the events socket (src/api/roster-feed.ts): the socket asks for them so, is told
 * each agent's entry once and then only what changed of it, and everything that listens for `harness` events
 * still hears each entry whole.
 */
import assert from "node:assert/strict";
import { test } from "node:test";

class Socket {
  static OPEN = 1;
  static all: Socket[] = [];
  readyState = 0;
  sent: string[] = [];
  onopen?: () => void;
  onmessage?: (ev: { data: string }) => void;
  onclose?: () => void;
  onerror?: () => void;
  constructor(readonly url: string) {
    Socket.all.push(this);
  }
  send(text: string) {
    this.sent.push(text);
  }
  close() {
    this.readyState = 3;
  }
  opened() {
    this.readyState = Socket.OPEN;
    this.onopen?.();
  }
  closed() {
    this.close();
    this.onclose?.();
  }
  /** A batch of `events`, as the server writes one. */
  batch(at: string, events: unknown[]) {
    this.onmessage?.({ data: JSON.stringify({ t: "batch", at, events }) });
  }
}

let timerId = 0;
const timers = new Map<number, () => void>();
Object.assign(globalThis, {
  WebSocket: Socket,
  location: { protocol: "http:", host: "localhost", pathname: "/" },
  document: Object.assign(new EventTarget(), { hidden: false }),
  window: Object.assign(new EventTarget(), {
    setTimeout: (fn: () => void) => {
      const id = ++timerId;
      timers.set(id, fn);
      return id;
    },
    clearTimeout: (id: number) => timers.delete(id),
  }),
});
const { events } = await import("../src/api/events.ts");
const { Roster, isRosterEvent } = await import("../src/api/roster-feed.ts");
type AgentInfo = import("../src/api/harness.ts").AgentInfo;

const latest = () => {
  const ws = Socket.all.at(-1);
  assert.ok(ws);
  return ws;
};
const entry = (id: string, more: Record<string, unknown> = {}) =>
  ({ id, kind: "claude", name: "Claude Code", status: "idle", queued: 0, approvals: [], last_activity: 1, ...more }) as unknown as AgentInfo;
const unreadOf = (ws: Socket) => ws.sent.map((s) => JSON.parse(s) as { t: string; of?: string; why?: string }).filter((f) => f.t === "unread");

await test("W21-R02 what changed of an agent's entry is applied to the entry the socket said: fields set, fields gone, each entry its own", () => {
  const roster = new Roster();
  const first = entry("a", { summary: "Done", doing: "cargo test" });
  roster.whole(first);
  const started = roster.changed({ type: "harness_patch", id: "a", set: { status: "running", last_activity: 2 }, unset: ["summary"] });
  assert.deepEqual(started, { type: "harness", info: { id: "a", kind: "claude", name: "Claude Code", status: "running", queued: 0, approvals: [], last_activity: 2, doing: "cargo test" } });
  assert.equal("summary" in first, true, "the entry it was said is as it was: the next is another");
  const over = roster.changed({ type: "harness_patch", id: "a", set: { status: "idle", summary: "Fixed", last_activity: 3 } });
  assert.equal(over?.info.summary, "Fixed");
  assert.equal(over?.info.status, "idle");
  // One never said, one gone, and after another socket or a resync: nothing to change.
  assert.equal(roster.changed({ type: "harness_patch", id: "b", set: { queued: 1 } }), undefined);
  roster.closed("a");
  assert.equal(roster.changed({ type: "harness_patch", id: "a", set: { queued: 1 } }), undefined);
  roster.whole(entry("a"));
  roster.clear();
  assert.equal(roster.changed({ type: "harness_patch", id: "a", set: { queued: 1 } }), undefined);
  // Told apart from every other event of a batch.
  assert.equal(isRosterEvent({ type: "harness_patch", id: "a", set: {} }), true);
  for (const other of [{ type: "harness", info: entry("a") }, { type: "harness_patch", id: "a" }, { type: "harness_patch", set: {} }, null, "harness_patch"]) assert.equal(isRosterEvent(other), false);
});

await test("W21-R02 the events socket asks for the agents lean, and its listeners hear each entry whole at every change", () => {
  const heard: AgentInfo[] = [];
  const closed: string[] = [];
  const off = events.on(["harness", "harness_closed"], (ev) => {
    if (ev.type === "harness") heard.push(ev.info);
    else if (ev.type === "harness_closed") closed.push(ev.id);
  });
  events.start();
  const ws = latest();
  try {
    assert.match(ws.url, /[?&]batch=1(&|$)/);
    assert.match(ws.url, /[?&]lean=1(&|$)/);
    ws.opened();
    ws.batch("7.1", [
      { type: "harness", info: entry("a", { summary: "Done" }) },
      { type: "harness", info: entry("b") },
    ]);
    ws.batch("7.2", [{ type: "harness_patch", id: "a", set: { status: "running", last_activity: 2 }, unset: ["summary"] }]);
    ws.batch("7.3", [{ type: "harness_patch", id: "a", set: { last_activity: 3 } }, { type: "updates" }, { type: "harness_patch", id: "b", set: { queued: 2 } }]);
    assert.deepEqual(
      heard.map((a) => [a.id, a.status, a.last_activity, a.summary, a.queued]),
      [
        ["a", "idle", 1, "Done", 0],
        ["b", "idle", 1, undefined, 0],
        ["a", "running", 2, undefined, 0],
        ["a", "running", 3, undefined, 0],
        ["b", "idle", 1, undefined, 2],
      ],
    );
    assert.equal(heard[2]?.kind, "claude", "what didn't change is as the socket said it");
    assert.deepEqual(unreadOf(ws), [], "every event was read");
    // An agent gone, then here again: its entry whole before what changes of it.
    ws.batch("7.4", [{ type: "harness_closed", id: "a" }]);
    assert.deepEqual(closed, ["a"]);
    ws.batch("7.5", [{ type: "harness_patch", id: "a", set: { queued: 1 } }]);
    assert.equal(heard.length, 5, "what changed of an entry this socket never said is not made up");
    assert.deepEqual(
      unreadOf(ws).map((f) => f.of),
      ["harness_patch"],
      "the server is told it wasn't read",
    );
    // Events missed (a resync): what the socket said before counts for nothing.
    ws.batch("7.6", [{ type: "resync" }, { type: "harness_patch", id: "b", set: { queued: 3 } }]);
    assert.equal(heard.length, 5);
    assert.equal(unreadOf(ws).length, 2);
    ws.batch("7.7", [
      { type: "harness", info: entry("b", { queued: 3 }) },
      { type: "harness_patch", id: "b", set: { queued: 4 } },
    ]);
    assert.deepEqual(
      heard.slice(5).map((a) => [a.id, a.queued]),
      [
        ["b", 3],
        ["b", 4],
      ],
    );
  } finally {
    off();
  }
});

await test("W21-R02 another socket starts over: what changes is of the entries it says itself", () => {
  const heard: AgentInfo[] = [];
  const off = events.on(["harness"], (ev) => {
    if (ev.type === "harness") heard.push(ev.info);
  });
  try {
    const before = latest();
    before.closed();
    // The socket is asked for again (at once when it had held, else at its timer).
    const next = timers.entries().next().value;
    if (Socket.all.at(-1) === before && next) {
      timers.delete(next[0]);
      next[1]();
    }
    const ws = latest();
    assert.notEqual(ws, before);
    assert.match(ws.url, /[?&]lean=1(&|$)/);
    assert.match(ws.url, /[?&]after=7\.7(&|$)/, "it resumes from where the last was");
    ws.opened();
    // The entry the socket before had said isn't this one's: what changed of it can't be applied.
    ws.batch("7.8", [{ type: "harness_patch", id: "b", set: { queued: 5 } }]);
    assert.equal(heard.length, 0);
    assert.equal(unreadOf(ws).length, 1);
    ws.batch("7.9", [
      { type: "harness", info: entry("b", { queued: 5 }) },
      { type: "harness_patch", id: "b", set: { queued: 6 } },
    ]);
    assert.deepEqual(
      heard.map((a) => a.queued),
      [5, 6],
    );
  } finally {
    off();
    events.stop();
  }
});
