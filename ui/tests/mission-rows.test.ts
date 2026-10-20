import assert from "node:assert/strict";
import { registerHooks } from "node:module";
import { test } from "node:test";
import type { AgentInfo, SubagentBrief } from "../src/api/harness.ts";
import { missionRows, type MissionRow } from "../src/views/harness/mission-rows.ts";

// Exercise the actual list matcher and sorter; their view imports need no CSS in Node.
const loader = registerHooks({
  load(url, context, nextLoad) {
    return url.endsWith(".css") ? { format: "module", source: "export {};", shortCircuit: true } : nextLoad(url, context);
  },
});
const { matches, sortRows } = await import("../src/views/harness/list-tools.tsx");
loader.deregister();

const task = (rest: Partial<SubagentBrief> = {}): SubagentBrief => ({
  id: "child", tool_id: "spawn", description: "Browser audit", status: "running", started: 2000, ...rest,
});
const agent = (rest: Partial<AgentInfo> = {}): AgentInfo => ({
  id: "owner", name: "Release manager", title: "Release work", kind: "codex", kind_name: "Codex",
  target: "agent:fedora", cwd: "/work/kiln", model: "model", mode: "fullAccess",
  created_at: 1000, last_activity: 9000, status: "running", approvals: [], queued: 0, tasks_running: 0, cost_usd: 0,
  ...rest,
});
const names = (rows: MissionRow[]) => rows.map((r) => r.a.name);

test("native names and states are searchable independently of the session containing them", () => {
  const rows = missionRows([{ a: agent({ subagents: [task({ status: "completed", ended: 3000 })] }), stand: "working" }]);
  assert.deepEqual(names(rows.filter((r) => matches("browser audit", {}, r))), ["Browser audit"]);
  assert.deepEqual(names(rows.filter((r) => matches("", { state: ["finished"] }, r))), ["Browser audit"]);
  assert.deepEqual(names(rows.filter((r) => matches("", { state: ["working"] }, r))), ["Release manager"]);
  assert.deepEqual(names(rows.filter((r) => matches("browser", { runner: ["fedora"], harness: ["codex"], folder: ["/work/kiln"], model: ["model"], mode: ["fullAccess"] }, r))), ["Browser audit"]);
});

test("a native task sorts by its own name and timestamps and can remain working after its session ends", () => {
  const rows = missionRows([
    { a: agent({ status: "exited", last_activity: 9000, subagents: [task({ description: "Alpha audit", started: 3000 })] }), stand: "ended" },
    { a: agent({ id: "peer", name: "Middle task", created_at: 2000, last_activity: 4000 }), stand: "working" },
  ]);
  assert.deepEqual(names(rows.filter((r) => matches("", { state: ["working"] }, r))), ["Alpha audit", "Middle task"]);
  assert.deepEqual(names(sortRows(rows, "newest")), ["Alpha audit", "Middle task", "Release manager"]);
  assert.deepEqual(names(sortRows(rows, "active")), ["Release manager", "Middle task", "Alpha audit"]);
  assert.deepEqual(names(sortRows(rows, "name")), ["Alpha audit", "Middle task", "Release manager"]);
});

test("native keys stay unique across sessions and delimiter-containing IDs without changing thread identities", () => {
  const rows = missionRows([
    { a: agent({ id: "a:b", subagents: [task({ id: "c/d" })] }), stand: "working" },
    { a: agent({ id: "a", subagents: [task({ id: "b:c/d" }), task({ id: "c/d" })] }), stand: "working" },
  ]);
  assert.equal(new Set(rows.map((r) => r.a.id)).size, rows.length);
  assert.deepEqual(rows.flatMap((r) => r.native ? [[r.native.agent, r.native.task.id]] : []), [["a:b", "c/d"], ["a", "b:c/d"], ["a", "c/d"]]);
  const changed = missionRows([{ a: agent({ id: "a:b", subagents: [task({ id: "c/d", description: "Renamed", status: "completed", ended: 7000 })] }), stand: "finished" }]);
  assert.equal(changed[1]?.a.id, rows[1]?.a.id);
});

test("native metadata carries no parent controls, usage, changes or grouping while real rows stay untouched", () => {
  const child = task({ parent_task: "grandparent", ended: 5000 });
  const owner = agent({
    cost_usd: 42, queued: 3, tasks_running: 8, files_changed: 10, lines_added: 200, lines_removed: 40,
    parent: "manager", plan_run: "plan", plan: "paid", account: "owner@example.test", subagents: [child],
    harness_session: "real-harness", goal: "Only the parent's assignment", todos_total: 5,
    workspace: { kind: "slates", over: "/work" },
    approvals: [{ id: "approve", tool: "shell", tool_name: "exec", title: "Run a command", at: 4000 }],
  });
  const managed: MissionRow = { a: owner, stand: "needs" };
  const rows = missionRows([managed]);
  assert.equal(rows[0], managed);
  assert.equal(rows[1]?.native?.task, child);
  const projected = rows[1]!.a;
  assert.deepEqual(projected.approvals, []);
  assert.deepEqual([projected.queued, projected.tasks_running, projected.cost_usd], [0, 0, 0]);
  for (const key of ["parent", "plan_run", "plan", "subagents", "harness_session", "workspace", "goal", "todos_total", "files_changed", "lines_added", "lines_removed", "account"]) {
    assert.equal(Object.hasOwn(projected, key), false, `native row inherited ${key}`);
  }
  assert.equal(owner.cost_usd, 42);
  assert.equal(owner.subagents?.[0], child);
});

test("native lifecycle states retain their distinctions and waiting children receive needs-you priority", () => {
  const states = ["running", "pending", "waiting", "paused", "completed", "failed", "stopped", "killed", "cancelled", "disconnected", "idle"];
  const rows = missionRows([{ a: agent({ subagents: states.map((status) => task({ id: status, description: status, status })) }), stand: "working" }]);
  assert.deepEqual(rows.slice(1).map((r) => r.stand), ["working", "working", "needs", "paused", "finished", "crashed", "ended", "ended", "ended", "offline", "ready"]);
  assert.equal(sortRows(rows, "name")[0]?.native?.task.id, "waiting");
  assert.deepEqual(names(rows.filter((r) => matches("", { state: ["ended"] }, r))), ["stopped", "killed", "cancelled"]);
});
