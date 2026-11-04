import assert from "node:assert/strict";
import { registerHooks } from "node:module";
import { test } from "node:test";
import type { AgentInfo, SaidHit, SubagentBrief } from "../src/api/harness.ts";
import type { AiSession } from "../src/api/types.ts";
import { missionFoundSessions, missionRows, missionSaid, standOf, updatedAt, activityAt, subagentsLine, type MissionRow } from "../src/views/harness/mission-rows.ts";
import { kilnAtWork, kilnSubagentsOf, subagentsAtWork } from "../src/views/harness/tasks.ts";

// Exercise the actual list matcher and sorter; their view imports need no CSS in Node.
const loader = registerHooks({
  load(url, context, nextLoad) {
    return url.endsWith(".css") ? { format: "module", source: "export {};", shortCircuit: true } : nextLoad(url, context);
  },
});
const { matches, sortRows, heldRows } = await import("../src/views/harness/list-tools.tsx");
loader.deregister();

const task = (rest: Partial<SubagentBrief> = {}): SubagentBrief => ({
  id: "native-child",
  tool_id: "spawn",
  description: "Browser audit",
  status: "running",
  started: 2000,
  ...rest,
});
const agent = (rest: Partial<AgentInfo> = {}): AgentInfo => ({
  id: "owner",
  name: "Release manager",
  title: "Release work",
  kind: "codex",
  kind_name: "Codex",
  target: "agent:fedora",
  cwd: "/work/kiln",
  model: "model",
  mode: "fullAccess",
  created_at: 1000,
  last_activity: 9000,
  status: "running",
  approvals: [],
  queued: 0,
  tasks_running: 0,
  owned_work_known: true,
  cost_usd: 0,
  ...rest,
});
const names = (rows: MissionRow[]) => rows.map((r) => r.a.name);

test("an agent started through kiln's API is a top-level row; a subagent never is", () => {
  const states = ["running", "pending", "waiting", "paused", "completed", "failed", "stopped", "killed", "cancelled", "disconnected", "idle"];
  for (const status of states) {
    // The roster as Mission Control lists it: every agent kiln runs, whoever started it.
    const rows: MissionRow[] = [
      { a: agent({ status: "exited", subagents: [task({ status })] }), stand: "ended" },
      { a: agent({ id: "peer", name: "Middle task", created_at: 2000 }), stand: "working" },
      { a: agent({ id: "started", parent: "owner", name: "Started by an agent", created_at: 3000 }), stand: "needs" },
    ];
    assert.deepEqual(
      rows.map((r) => r.a.id),
      ["owner", "peer", "started"],
      status,
    );
    // The subagent is no row, no count and no facet: its words find nothing.
    assert.deepEqual(
      rows.filter((r) => matches("browser audit", {}, r)),
      [],
    );
    // The agent another agent started counts, filters and sorts like any other.
    assert.deepEqual(names(rows.filter((r) => matches("", { state: ["needs"] }, r))), ["Started by an agent"]);
    assert.deepEqual(names(rows.filter((r) => matches("started by", {}, r))), ["Started by an agent"]);
    assert.deepEqual(names(rows.filter((r) => matches("", { state: ["working"] }, r))), ["Middle task"]);
    assert.deepEqual(names(sortRows(rows, "newest")), ["Started by an agent", "Middle task", "Release manager"]);
  }
});

test("sessions found on the machines never list kiln's own agents or their subagents again", () => {
  const session = (id: string, managed_by?: string): AiSession => ({
    id,
    managed_by,
    machine: "fedora",
    tool: "codex",
    pid: 1,
    name: id,
    title: id,
    cwd: "/work",
    branch: "main",
    model: "model",
    started_at: 1,
    last_activity: 2,
    state: "idle",
    last_prompt: "",
    last_reply: "",
    terminal: null,
    transcript: true,
  });
  const all = [{ a: agent({ harness_session: "owner-session", subagents: [task()] }) }, { a: agent({ id: "child", parent: "owner", harness_session: "child-session" }) }];
  const found = [session("owner-session"), session("child-session"), session("native-child"), session("other-managed", "other"), session("external")];
  assert.deepEqual(
    missionFoundSessions(found, all).map((s) => s.id),
    ["external"],
  );
});

test("the subagents kiln starts for an agent's goals or plan steps are on its row, not rows of their own", () => {
  const ask = [{ id: "ask-1", tool: "shell" as const, tool_name: "Bash", title: "cargo test", at: 5000 }];
  const main = agent({ id: "main", name: "Merge Cancelling", status: "idle", last_activity: 1000 });
  const step = (id: string, name: string, rest: Partial<AgentInfo> = {}) => agent({ id, name, parent: "main", subagent: true, status: "running", created_at: 4000, ...rest });
  const started = agent({ id: "helper", name: "Started by an agent", parent: "main", status: "idle", last_activity: 1000 });
  const list = [main, step("s2", "Step 2: API"), step("s3", "Step 3: UI", { status: "idle" }), started];
  // Its turn over, it works through its step's subagent as through its harness's own; the step
  // whose turn is over isn't at work. The agent it started through kiln's API is a row of its own.
  const rows = missionRows(list, {});
  assert.deepEqual(
    rows.map((r) => [r.a.id, r.stand]),
    [
      ["main", "working"],
      ["helper", "finished"],
    ],
  );
  const row = rows[0];
  assert.ok(row && !row.asking);
  assert.equal(subagentsLine(subagentsAtWork(row.a.subagents)), "1 subagent · Step 2: API");
  assert.deepEqual(
    kilnSubagentsOf(list, "main").map((a) => a.id),
    ["s2", "s3"],
  );
  assert.deepEqual(
    kilnAtWork(kilnSubagentsOf(list, "main")).map((a) => a.id),
    ["s2"],
  );
  // One that waits on you puts what it asks on its agent's row, answered as its own.
  const asking = missionRows([main, step("s2", "Step 2: API", { status: "waiting", approvals: ask })], {});
  assert.deepEqual(
    asking.map((r) => [r.a.id, r.stand, r.asking?.id]),
    [["main", "needs", "s2"]],
  );
  // The agent's own request comes first: its row is its.
  const both = missionRows([agent({ ...main, status: "waiting", approvals: ask }), step("s2", "Step 2: API", { status: "waiting", approvals: ask })], {});
  assert.deepEqual(
    both.map((r) => [r.a.id, r.stand, r.asking]),
    [["main", "needs", undefined]],
  );
  // Its agent gone, a subagent is a row of its own, so nothing goes unseen.
  assert.deepEqual(
    missionRows([step("s2", "Step 2: API")], {}).map((r) => r.a.id),
    ["s2"],
  );
});

test("conversation search covers agents started through kiln's API, but not subagent thread events", () => {
  const hit = (id: string, agent: string, parent?: string): SaidHit => ({
    agent,
    name: agent,
    machine: "fedora",
    event: { id, seq: 1, at: 1, kind: "assistant", text: "Release findings", parent },
  });
  const hits = [hit("root", "owner"), hit("native", "owner", "native-child"), hit("managed", "child"), hit("unknown", "missing"), hit("ended", "ended-root")];
  assert.deepEqual(
    missionSaid(hits, new Set(["owner", "child", "ended-root"])).map((h) => h.event.id),
    ["root", "managed", "ended"],
  );
  assert.equal(hits.length, 5);
});

test("a sort chosen on purpose is followed as it is; by default what needs you comes first, longest waiting first", () => {
  const ask = (at: number) => [{ id: `ask-${at}`, tool: "shell" as const, tool_name: "Bash", title: "cargo test", at }];
  const rows: MissionRow[] = [
    { a: agent({ id: "beta", name: "Beta" }), stand: "working" },
    { a: agent({ id: "alpha", name: "Alpha" }), stand: "finished" },
    { a: agent({ id: "gamma", name: "Gamma", status: "waiting", approvals: ask(5000) }), stand: "needs" },
    { a: agent({ id: "delta", name: "Delta", status: "waiting", approvals: ask(3000) }), stand: "needs" },
    { a: agent({ id: "epsilon", name: "Epsilon", status: "exited" }), stand: "crashed" },
  ];
  assert.deepEqual(names(sortRows(rows, "name")), ["Alpha", "Beta", "Delta", "Epsilon", "Gamma"]);
  assert.deepEqual(names(sortRows(rows, "priority")), ["Delta", "Gamma", "Epsilon", "Alpha", "Beta"]);
});

test("mc_attention: needs-first follows server ranking below urgent rows", () => {
  const states = ["ended", "offline", "paused", "crashed", "finished", "ready", "working", "needs"] as const;
  const rows: MissionRow[] = states.map((stand, i) => ({
    a: agent({ id: stand, name: stand, created_at: 9000 - i * 1000, last_activity: 9000 - i * 1000 }),
    stand,
    rank: ["needs", "working", "ready", "finished", "crashed", "paused", "offline", "ended"].indexOf(stand),
  }));
  assert.deepEqual(
    sortRows(rows, "priority").map((r) => r.stand),
    ["needs", "crashed", "working", "ready", "finished", "paused", "offline", "ended"],
  );
  assert.deepEqual(
    rows.map((r) => r.stand),
    states,
    "sorting does not mutate the live roster",
  );
});

test("mc_attention: lifecycle changes preserve the sampled order except needs-you", () => {
  const rows: MissionRow[] = [
    { a: agent({ id: "work", name: "Work", created_at: 1000 }), stand: "working", rank: 0 },
    { a: agent({ id: "ready", name: "Ready", created_at: 2000 }), stand: "ready", rank: 1 },
    { a: agent({ id: "done", name: "Done", created_at: 3000, last_activity: 10000 }), stand: "finished", rank: 2 },
  ];
  assert.deepEqual(names(sortRows(rows, "priority")), ["Work", "Ready", "Done"]);
  const updated = rows.map((r): MissionRow => (r.a.id === "done" ? { ...r, stand: "working" } : r.a.id === "work" ? { ...r, stand: "finished" } : r));
  assert.deepEqual(names(sortRows(updated, "priority")), ["Work", "Ready", "Done"]);
  const needs = updated.map((r): MissionRow => (r.a.id === "work" ? { ...r, stand: "needs" } : r));
  assert.deepEqual(names(sortRows(needs, "priority")), ["Work", "Ready", "Done"]);
});

// Mission Control attention plan 01a199e1b177752e84dad5dc990e6a15.
test("mc_attention: Finished requires zero work; Updated does not change status", () => {
  const quiet = agent({ status: "idle", tasks_running: 0, last_turn: { at: 8000, how: "done" } });
  for (const seen of [0, 8000, 20000]) assert.equal(standOf(quiet, seen), "finished");
  assert.equal(standOf({ ...quiet, tasks_running: 1 }), "ready");
  assert.equal(standOf({ ...quiet, left_behind: 1 }), "ready");
  assert.equal(standOf({ ...quiet, subagents: [task()] }), "working");
  assert.equal(missionRows([quiet], {}).at(0)?.updated, true);
  assert.equal(missionRows([quiet], { owner: 8000 }).at(0)?.updated, false);
  const poll = { ...quiet, last_activity: 999999999, created_at: 999999999 };
  assert.equal(updatedAt(poll), 8000);
  assert.equal(activityAt(poll), 8000);
  assert.equal(missionRows([poll], { owner: 8000 }).at(0)?.updated, false);
});

test("mc_attention: idle work with an unknown legacy ownership inventory stays Ready", () => {
  for (const owned_work_known of [undefined, false]) {
    assert.equal(standOf(agent({ status: "idle", owned_work_known }), 0), "ready");
  }
});

test("mc_attention: every explicit Kiln descendant must confirm ownership before Finished", () => {
  const main = agent({ status: "idle" });
  const child = agent({ id: "child", parent: "owner", subagent: true, status: "idle" });
  const nested = agent({ id: "nested", parent: "child", subagent: true, status: "idle", owned_work_known: undefined });
  assert.equal(missionRows([main, child, nested], {}).at(0)?.stand, "ready");
  assert.equal(missionRows([main, child, { ...nested, owned_work_known: true }], {}).at(0)?.stand, "finished");
  assert.equal(missionRows([main, { ...child, subagent: false, owned_work_known: undefined }, nested], {}).at(0)?.stand, "finished");
});

test("mc_attention: idle subagent background work prevents Finished; standalone work does not", () => {
  const main = agent({ status: "idle", tasks_running: 0 });
  const child = agent({ id: "kid", parent: "owner", subagent: true, status: "idle", tasks_running: 1, last_turn: { at: 12000, how: "done" } });
  const rows = missionRows([main, child], {});
  assert.deepEqual(
    rows.map((r) => [r.a.id, r.stand]),
    [["owner", "ready"]],
  );
  assert.equal(rows.at(0)?.updated, true);
  const standalone = { ...child, subagent: false, status: "running" as const };
  assert.deepEqual(
    missionRows([main, standalone], {}).map((r) => [r.a.id, r.stand]),
    [
      ["owner", "finished"],
      ["kid", "working"],
    ],
  );
});

test("mc_attention: nested subagents keep their own row's parent working and updated", () => {
  const parent = agent({ status: "idle", tasks_running: 0 });
  const child = agent({ id: "child", parent: "owner", subagent: true, status: "idle", tasks_running: 0 });
  const grandchild = agent({ id: "grandchild", parent: "child", subagent: true, status: "running" });
  assert.deepEqual(
    missionRows([parent, child, grandchild], {}).map((r) => [r.a.id, r.stand]),
    [["owner", "working"]],
  );
  const done = { ...grandchild, status: "idle" as const, tasks_running: 0, last_turn: { at: 15000, how: "done" } };
  const [row] = missionRows([parent, child, done], { owner: 10000 });
  assert.equal(row?.stand, "finished");
  assert.equal(row?.updated, true);
  assert.equal(row ? updatedAt(row.a) : 0, 15000);
});

test("mc_attention: errors remain prominent and newly urgent ended rows escape a held fold", () => {
  const normal: MissionRow = { a: agent({ id: "normal" }), stand: "working", rank: 0 };
  const error: MissionRow = { a: agent({ id: "error", status: "exited", exit: { code: 1, error: "Cannot restart", at: 10000 } }), stand: "crashed", rank: 999 };
  const needs: MissionRow = { a: agent({ id: "needs", status: "waiting" }), stand: "needs", rank: 999 };
  assert.deepEqual(
    sortRows([normal, error, needs], "priority").map((r) => r.a.id),
    ["needs", "error", "normal"],
  );
  const held = { ids: ["normal", "error", "needs"], ended: new Set(["error", "needs"]) };
  assert.deepEqual(
    heldRows([normal, error, needs], [needs, error, normal], held).live.map((r) => r.a.id),
    ["needs", "error", "normal"],
  );
  assert.equal(heldRows([normal, error, needs], [needs, error, normal], held).ended.length, 0);
});

test("mc_attention: urgent rows follow the server's globally comparable request rank", () => {
  const old: MissionRow = {
    a: agent({ id: "old", status: "waiting", approvals: [{ id: "o", tool: "question", tool_name: "question", title: "Old request", at: 10 }] }),
    stand: "needs",
    requestRank: 1,
  };
  const recent: MissionRow = {
    a: agent({ id: "recent", status: "waiting", approvals: [{ id: "n", tool: "question", tool_name: "question", title: "New request", at: 20 }] }),
    stand: "needs",
    requestRank: 0,
  };
  assert.deepEqual(
    sortRows([old, recent], "priority").map((r) => r.a.id),
    ["recent", "old"],
  );
  assert.deepEqual(
    sortRows(
      [
        { ...old, requestRank: undefined },
        { ...recent, requestRank: undefined },
      ],
      "priority",
    ).map((r) => r.a.id),
    ["old", "recent"],
  );
});
