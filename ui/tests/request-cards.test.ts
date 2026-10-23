import assert from "node:assert/strict";
import { test } from "node:test";
import type { AgentInfo, Pause } from "../src/api/harness.ts";
import { cardsOf, heldWords } from "../src/views/harness/cards.ts";

const agent = (id: string, more: Partial<AgentInfo> = {}): AgentInfo => ({
  id,
  kind: "claude",
  kind_name: "Claude Code",
  target: "local",
  name: id,
  cwd: "/work",
  created_at: 0,
  status: "idle",
  title: "",
  model: "",
  mode: "default",
  approvals: [],
  queued: 0,
  tasks_running: 0,
  cost_usd: 0,
  last_activity: 1_000,
  ...more,
});
const limit = (since: number): Pause => ({ reason: "limit", label: "Session", said: "", since, auto: true, by: "harness" });
const none = new Set<string>();
const kinds = (list: AgentInfo[], path = "/runs", away = none, answered = none) => cardsOf(list, path, away, answered).map((c) => `${c.kind}:${c.a.id}`);

const fleet = [
  agent("lim", { pause: limit(50) }),
  agent("crash", { status: "exited", exit: { code: 127, error: "codex: command not found" }, last_activity: 40 }),
  agent("plan", { held: { why: 'Paused by the agent: plan "Q" is ready for you to look at', plan: "pl_q", since: 30 } }),
  agent("ask", { status: "waiting", approvals: [{ id: "q1", tool: "question", tool_name: "", title: "Postgres or SQLite?", at: 60 }] }),
  agent("both", { held: { why: "Paused by the agent.", since: 10 }, approvals: [{ id: "b1", tool: "shell", tool_name: "Bash", title: "ls", at: 70 }] }),
  agent("idle"),
  agent("mine", { status: "exited", exit: { code: 0 } }),
];

test("one card an agent, the most pressing first: requests, goals paused for you, stops of their own, usage limits", () => {
  assert.deepEqual(kinds(fleet), ["request:ask", "request:both", "held:plan", "stopped:crash", "limit:lim"]);
});

test("none where the page shows it itself: Mission control, and the agent's own pages", () => {
  assert.deepEqual(kinds(fleet, "/agents"), []);
  assert.deepEqual(kinds(fleet, "/agents/plan"), ["request:ask", "request:both", "stopped:crash", "limit:lim"]);
  assert.deepEqual(kinds(fleet, "/agents/lim/changes"), ["request:ask", "request:both", "held:plan", "stopped:crash"]);
});

test("a limit under a turn still going isn't one yet, nor a limit of a machine gone quiet", () => {
  assert.deepEqual(kinds([agent("a", { status: "running", pause: limit(5) }), agent("b", { status: "disconnected", pause: limit(5) })]), []);
});

test("put away, a card stays away until there's something new", () => {
  const away = new Set(["held:plan:30", "limit:lim:50"]);
  assert.deepEqual(kinds(fleet, "/runs", away), ["request:ask", "request:both", "stopped:crash"]);
  const again = [agent("plan", { held: { why: "Paused by the agent: once more", since: 31 } }), agent("lim", { pause: limit(51) })];
  assert.deepEqual(kinds(again, "/runs", away), ["held:plan", "limit:lim"]);
});

test("a request answered here goes at once; its agent's next one shows, and then what else it has", () => {
  const two = agent("two", {
    held: { why: "Paused by the agent.", since: 1 },
    approvals: [
      { id: "x1", tool: "shell", tool_name: "Bash", title: "make", at: 5 },
      { id: "x2", tool: "edit", tool_name: "Edit", title: "a.rs", at: 6 },
    ],
  });
  const first = cardsOf([two], "/runs", none, none);
  assert.deepEqual(
    first.map((c) => [c.kind, c.key, c.more]),
    [["request", "x1", 1]],
  );
  assert.deepEqual(
    cardsOf([two], "/runs", none, new Set(["x1"])).map((c) => [c.kind, c.key, c.more]),
    [["request", "x2", 0]],
  );
  assert.deepEqual(kinds([two], "/runs", none, new Set(["x1", "x2"])), ["held:two"]);
});

test("a held stack's why, without who paused it", () => {
  assert.equal(heldWords('Paused by the agent: plan "Q" is ready for you to look at'), 'plan "Q" is ready for you to look at');
  assert.equal(heldWords("Paused by Release prep: waiting on the staging password"), "waiting on the staging password");
  assert.equal(heldWords("Paused by the agent."), "");
});

test("a stop is known by when it ended: put away, it stays away as the agent's activity moves on; a later stop is new", () => {
  const stopped = (last: number, at: number) => agent("crash", { status: "exited", exit: { code: 1, error: "boom", at }, last_activity: last });
  const [card] = cardsOf([stopped(40, 35)], "/runs", none, none);
  assert.equal(card?.key, "stopped:crash:35");
  const away = new Set([card?.key ?? ""]);
  assert.deepEqual(kinds([stopped(90, 35)], "/runs", away), [], "a refresh, a restart, its activity since: the same stop");
  assert.deepEqual(kinds([stopped(120, 110)], "/runs", away), ["stopped:crash"], "it stopped again later");
});
