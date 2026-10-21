import assert from "node:assert/strict";
import { test } from "node:test";
import type { GoalItem, GoalStack } from "../src/api/harness.ts";
import { fromLines, stackReads, startWords, stoppedBy, toGo } from "../src/views/harness/goal-stack.ts";

const goal = (id: string, text: string, status: GoalItem["status"] = "pending"): GoalItem => ({ id, text, status });
const stack = (rest: Partial<GoalStack> = {}): GoalStack => ({ agent: "a1", state: "running", items: [], updated_at: 1, ...rest });

// The screenshot's stack: one skipped, one under way, four waiting.
const items = [
  goal("s", "Beat Splinter DB", "skipped"),
  goal("a", "Add QUIC replication", "active"),
  goal("b", "Build the storage layer"),
  goal("c", "Optimize the APIs"),
  goal("d", "Ship the binary"),
];

test("a paused stack with its goal under way resumes it, though a goal before was skipped", () => {
  // The screenshot's stack, paused: Resume, not "Go on to the next" (which read as if it would skip it).
  const paused = stack({ state: "paused", items });
  assert.equal(startWords(paused), "Resume");
  assert.equal(stoppedBy(paused), undefined);
  assert.equal(startWords(stack({ items })), null, "running: no start button");
  // Stopped by one not met, none under way: go on to the next, or try that one again.
  const stopped = stack({ state: "paused", items: [goal("m", "lint passes", "met"), goal("u", "fly", "unmet"), goal("b", "docs build")] });
  assert.equal(startWords(stopped), "Go on to the next");
  assert.equal(stoppedBy(stopped)?.id, "u");
  // One skipped long ago, then one met: nothing stopped it.
  const later = stack({ state: "paused", items: [goal("s", "fly", "skipped"), goal("m", "lint passes", "met"), goal("b", "docs build")] });
  assert.equal(startWords(later), "Resume");
  assert.equal(stoppedBy(later), undefined);
  // Not started yet; done (nothing to go); none at all.
  assert.equal(startWords(stack({ state: "paused", items: [goal("b", "docs build")] })), "Start");
  assert.equal(startWords(stack({ state: "done", items: [goal("m", "lint passes", "met")] })), null);
  assert.equal(startWords(null), null);
});

test("an edit covers the goal under way first, then the goals to come, never the ones done", () => {
  assert.deepEqual(
    toGo(items).map((g) => g.id),
    ["a", "b", "c", "d"],
  );
  assert.deepEqual(toGo([goal("m", "x", "met"), goal("u", "y", "unmet")]), []);
});

test("lines moved keep their goals: another put first, the goal under way goes back among them", () => {
  const edit = toGo(items);
  const { goals, dropped } = fromLines("Optimize the APIs\nAdd QUIC replication\nBuild the storage layer\nShip the binary", edit);
  assert.deepEqual(goals, [
    { id: "c", text: "Optimize the APIs" },
    { id: "a", text: "Add QUIC replication" },
    { id: "b", text: "Build the storage layer" },
    { id: "d", text: "Ship the binary" },
  ]);
  assert.equal(dropped, null);
});

test("a goal reworded keeps its place and its goal; spacing and blank lines don't matter", () => {
  const edit = toGo(items);
  // The goal under way given other words, the first line.
  assert.deepEqual(fromLines("Add QUIC and UDP replication\n\nBuild the storage layer\n  Optimize   the APIs \nShip the binary\n", edit).goals, [
    { id: "a", text: "Add QUIC and UDP replication" },
    { id: "b", text: "Build the storage layer" },
    { id: "c", text: "Optimize the APIs" },
    { id: "d", text: "Ship the binary" },
  ]);
  // A goal to come reworded, and a new one: the changed line takes the goal left, the rest are new.
  assert.deepEqual(fromLines("Add QUIC replication\nBuild the storage layer, with tests\nOptimize the APIs\nShip the binary\nWrite the docs", edit).goals, [
    { id: "a", text: "Add QUIC replication" },
    { id: "b", text: "Build the storage layer, with tests" },
    { id: "c", text: "Optimize the APIs" },
    { id: "d", text: "Ship the binary" },
    { text: "Write the docs" },
  ]);
});

test("the goal under way's line taken out is to be skipped; goals to come taken out go", () => {
  const edit = toGo(items);
  const out = fromLines("Build the storage layer\nShip the binary", edit);
  assert.deepEqual(out.goals, [
    { id: "b", text: "Build the storage layer" },
    { id: "d", text: "Ship the binary" },
  ]);
  assert.equal(out.dropped?.id, "a");
  // A new goal put first, the goal under way's line kept below: it goes back, the new one goes next.
  const first = fromLines("Hotfix the build\nAdd QUIC replication\nBuild the storage layer\nOptimize the APIs\nShip the binary", edit);
  assert.deepEqual(first.goals[0], { text: "Hotfix the build" });
  assert.deepEqual(first.goals[1], { id: "a", text: "Add QUIC replication" });
  assert.equal(first.dropped, null);
});

test("a goal said twice is one goal and a new one; with no goal under way, nothing is to be skipped", () => {
  const edit = [goal("b", "docs build"), goal("c", "ship it")];
  const { goals, dropped } = fromLines("docs build\ndocs build\nship it", edit);
  assert.deepEqual(goals, [{ id: "b", text: "docs build" }, { text: "docs build" }, { id: "c", text: "ship it" }]);
  assert.equal(dropped, null);
});

test("a read asked for before a change here came back doesn't undo the change; the next one shows what's new", () => {
  let now = 0;
  const reads = stackReads(() => now);
  const running = stack({ items: [goal("a", "tests pass", "active")] });
  const paused = stack({ state: "paused", items: [goal("a", "tests pass", "active")] });
  const sent = stack({ state: "paused", items: [goal("a", "tests pass", "active"), goal("b", "docs build")] });
  assert.equal(reads.last("a1"), null);
  now = 1;
  assert.equal(reads.ask("a1")(running), running);
  // A poll goes out, then Pause comes back, then the poll lands (answered before the pause).
  now = 2;
  const poll = reads.ask("a1");
  now = 3;
  reads.changed("a1", paused);
  now = 4;
  assert.equal(poll(running), paused);
  // Asked for after it: what the server says now.
  now = 5;
  assert.equal(reads.ask("a1")(sent), sent);
  assert.equal(reads.last("a1"), sent);
  // Another agent's reads are its own.
  assert.equal(reads.ask("a2")(running), running);
});
