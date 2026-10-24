import assert from "node:assert/strict";
import { test } from "node:test";
import type { GoalItem, GoalStack, HEvent, Task } from "../src/api/harness.ts";
import { callWords, canPlan, canPlanNow, fromLines, plannerOf, stackReads, startWords, stepStand, stoppedBy, toGo } from "../src/views/harness/goal-stack.ts";

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

test("a planned goal's steps stand as their parts do: under way while one of its criteria is", () => {
  const g = goal("g", "Ship the replication");
  const step = (id: string, n: number, status: GoalItem["status"]): GoalItem => ({ ...goal(id, `Step ${n}`, status), part_of: "g", part_kind: "plan", step: { plan: "p", version: 1, n } });
  const items = [step("s1", 1, "met"), step("s2", 2, "pending"), { ...goal("c1", "fills stay ahead", "active"), part_of: "s2", part_kind: "checks" as const }, step("s3", 3, "pending"), g];
  assert.deepEqual(
    [1, 2, 3, 4].map((n) => stepStand(g, items, n, false)),
    ["met", "working", "waiting", undefined],
  );
  assert.equal(stepStand(g, items, 2, true), "paused", "the stack paused: the step under way is paused");
  assert.equal(canPlan(g), true);
  assert.equal(canPlan(items[0]!), false, "a plan's step isn't planned on its own");
  assert.equal(canPlan({ ...g, planned_as: "p" }), false, "nor a goal planned already");
  assert.equal(canPlan({ ...g, status: "met" }), false, "nor one met");
});

test("a goal being planned, its planning waiting its turn, can be planned now by a subagent; one under way or planned can't", () => {
  const g = { ...goal("g", "Build the storage layer"), planned_as: "p1" };
  const planning = (status: GoalItem["status"]): GoalItem => ({ ...goal("pl", "Plan it", status), part_of: "g", part_kind: "planning" });
  assert.equal(canPlanNow(g, [planning("pending"), g]), true);
  assert.equal(canPlanNow(g, [planning("active"), g]), false, "its planning under way already");
  assert.equal(canPlanNow(g, [planning("met"), g]), false, "planned");
  assert.equal(canPlanNow({ ...g, planned_as: undefined }, [g]), false, "not being planned: Make a plan");
});

// Who's planning a goal (Make a plan, Plan it now), as the agent's session shows it.
const NOW = 10_000_000;
const brief = (plan: string) => `The brief: plan ${plan}, research rs1, for the goal "Build the storage layer".`;
const sub = (id: string, plan: string, status: string, started: number, rest: Partial<Task> = {}): Task => ({
  id,
  tool_id: `tu-${id}`,
  description: "Plan it",
  class: "agent",
  status,
  started,
  prompt: brief(plan),
  ...rest,
});
let seq = 0;
const call = (parent: string | undefined, name: string, title: string, rest: Record<string, unknown> = {}): HEvent =>
  ({ id: `e${++seq}`, seq, at: NOW - 1000, parent, kind: "tool", tool_id: `c${seq}`, name, tool: "read", title, input: {}, status: "done", started: NOW - 1000, ...rest }) as HEvent;
const asked = (plan: string, at: number): HEvent => ({ id: `u${++seq}`, seq, at, kind: "user", text: `Your person asked… The brief: plan ${plan}, research rs1.`, client_id: `kiln-plan-${seq}` });

test("a subagent whose brief names the plan is its planner: its last calls, oldest first, none of another's", () => {
  const tasks = [sub("t1", "p1", "running", NOW - 60_000), sub("t2", "p2", "running", NOW - 50_000), { ...sub("t3", "p1", "running", NOW - 40_000), class: "shell" as const }];
  const events = [
    call("tu-t1", "Read", "a.rs"),
    call("tu-t2", "Read", "other.rs"),
    call(undefined, "Read", "main thread.rs"),
    call("tu-t1", "Read", "b.rs"),
    call("tu-t1", "Read", "c.rs"),
    call("tu-t1", "Read", "d.rs"),
  ];
  const p = plannerOf("p1", tasks, events, [], NOW);
  assert.equal(p?.kind, "planning");
  assert.ok(p && p.kind !== "asked");
  assert.equal(p.task.id, "t1", "a shell naming the plan isn't a planner");
  assert.deepEqual(
    p.calls.map((c) => (c.kind === "tool" ? c.title : "")),
    ["b.rs", "c.rs", "d.rs"],
  );
  assert.equal(plannerOf("p3", tasks, events, [], NOW), null, "none for a plan nobody plans");
});

test("the latest planner counts; one that stopped short is ended, one that said the plan is ready is none", () => {
  const again = [sub("t1", "p1", "completed", NOW - 600_000, { ended: NOW - 500_000 }), sub("t2", "p1", "running", NOW - 60_000)];
  assert.equal(plannerOf("p1", again, [], [], NOW)?.kind, "planning", "sent again: the new one");
  const stopped = [sub("t1", "p1", "completed", NOW - 600_000, { ended: NOW - 500_000, summary: "I couldn't reach the bench box." })];
  assert.equal(plannerOf("p1", stopped, [call("tu-t1", "mcp__kiln__research_add", "kiln · research_add", { server: "kiln", tool_short: "research_add" })], [], NOW)?.kind, "ended");
  const ready = [call("tu-t1", "mcp__kiln__plans_ready", "kiln · plans_ready", { server: "kiln", tool_short: "plans_ready" }), ...["a", "b", "c", "d"].map((f) => call("tu-t1", "Read", f))];
  assert.equal(plannerOf("p1", stopped, ready, [], NOW), null, "it said the plan is ready, before its last calls: the stack shows the steps");
});

test("kiln's ask stands until a subagent starts, half an hour at most, said or still queued", () => {
  assert.deepEqual(plannerOf("p1", [], [asked("p1", NOW - 20_000)], [], NOW), { kind: "asked", at: NOW - 20_000 });
  assert.equal(plannerOf("p1", [], [asked("p1", NOW - 31 * 60_000)], [], NOW), null, "past half an hour: plan it now again");
  assert.equal(plannerOf("p2", [], [asked("p1", NOW - 20_000)], [], NOW), null, "another plan's ask");
  const person: HEvent = { id: "u-person", seq: 99, at: NOW - 1000, kind: "user", text: "plan p1 please", client_id: "web-1" };
  assert.equal(plannerOf("p1", [], [person], [], NOW), null, "a person's own words aren't kiln's ask");
  assert.deepEqual(
    plannerOf("p1", [], [], [{ id: "kiln-plan-q1", text: "The brief: plan p1, research rs1.", at: NOW - 5000 }], NOW),
    { kind: "asked", at: NOW - 5000 },
    "queued behind the turn under way",
  );
});

test("a planner's calls, in plain words: what it's at, and what kiln's own tools did", () => {
  const c = (rest: Partial<Parameters<typeof callWords>[0]>) => callWords({ tool: "other", name: "X", title: "", status: "done", input: {}, ...rest });
  assert.equal(c({ tool: "read", name: "Read", title: "crates/a.rs", status: "running" }), "Reading crates/a.rs");
  assert.equal(c({ tool: "read", name: "Read", title: "crates/a.rs" }), "Read crates/a.rs");
  assert.equal(c({ tool: "web", name: "WebSearch", title: "DPhyp join order" }), "Searched the web for “DPhyp join order”");
  assert.equal(c({ tool: "web", name: "WebFetch", title: "https://duckdb.org/x", status: "running" }), "Reading https://duckdb.org/x");
  assert.equal(c({ tool: "search", name: "Grep", title: "fn join_order" }), "Searched for fn join_order");
  assert.equal(c({ tool: "mcp", server: "kiln", tool_short: "research_add", input: { findings: [{ claim: "DuckDB uses DPhyp." }] } }), "Added a finding: DuckDB uses DPhyp.");
  assert.equal(c({ tool: "mcp", server: "kiln", tool_short: "research_add", input: { findings: [{ claim: "A." }, { claim: "B." }] }, status: "running" }), "Adding 2 findings: A.");
  assert.equal(c({ tool: "mcp", server: "kiln", tool_short: "plans_update", input: { items: [{ title: "Measure" }, { title: "Fix" }] } }), "Wrote 2 steps: Measure · Fix");
  assert.equal(c({ tool: "mcp", server: "kiln", tool_short: "plans_ready" }), "Said the plan is ready");
  assert.equal(c({ tool: "mcp", server: "github", tool_short: "search", title: "github · search" }), "search: github · search", "another server's: its tool and title");
});
