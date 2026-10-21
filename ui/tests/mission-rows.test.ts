import assert from "node:assert/strict";
import { registerHooks } from "node:module";
import { test } from "node:test";
import type { AgentInfo, SaidHit, SubagentBrief } from "../src/api/harness.ts";
import type { AiSession } from "../src/api/types.ts";
import { missionFoundSessions, missionRows, missionSaid, type MissionRow } from "../src/views/harness/mission-rows.ts";

// Exercise the actual list matcher and sorter; their view imports need no CSS in Node.
const loader = registerHooks({
  load(url, context, nextLoad) {
    return url.endsWith(".css") ? { format: "module", source: "export {};", shortCircuit: true } : nextLoad(url, context);
  },
});
const { matches, sortRows } = await import("../src/views/harness/list-tools.tsx");
loader.deregister();

const task = (rest: Partial<SubagentBrief> = {}): SubagentBrief => ({
  id: "native-child", tool_id: "spawn", description: "Browser audit", status: "running", started: 2000, ...rest,
});
const agent = (rest: Partial<AgentInfo> = {}): AgentInfo => ({
  id: "owner", name: "Release manager", title: "Release work", kind: "codex", kind_name: "Codex",
  target: "agent:fedora", cwd: "/work/kiln", model: "model", mode: "fullAccess",
  created_at: 1000, last_activity: 9000, status: "running", approvals: [], queued: 0, tasks_running: 0, cost_usd: 0,
  ...rest,
});
const names = (rows: MissionRow[]) => rows.map((r) => r.a.name);

test("Mission Control includes only standalone agents, even when a child's parent is absent", () => {
  const rows: MissionRow[] = [
    { a: agent({ subagents: [task()] }), stand: "working" },
    { a: agent({ id: "child", parent: "owner" }), stand: "working" },
    { a: agent({ id: "grandchild", parent: "child" }), stand: "needs" },
    { a: agent({ id: "orphan", parent: "missing" }), stand: "finished" },
    { a: agent({ id: "plan-root", plan_run: "plan" }), stand: "ready" },
    { a: agent({ id: "attachment", harness_session: "external-session", parent: "" }), stand: "ready" },
    { a: agent({ id: "offline-root", status: "disconnected" }), stand: "offline" },
    { a: agent({ id: "ended-root", status: "exited" }), stand: "ended" },
    { a: agent({ id: "named-root", name: "/root/a-name-is-not-a-relationship" }), stand: "working" },
  ];
  assert.deepEqual(missionRows(rows).map((r) => r.a.id), ["owner", "plan-root", "attachment", "offline-root", "ended-root", "named-root"]);
});

test("native lifecycle updates never add rows, counts, filter facets or action targets", () => {
  const states = ["running", "pending", "waiting", "paused", "completed", "failed", "stopped", "killed", "cancelled", "disconnected", "idle"];
  for (const status of states) {
    const rows = missionRows([
      { a: agent({ status: "exited", subagents: [task({ status })] }), stand: "ended" },
      { a: agent({ id: "peer", name: "Middle task", created_at: 2000 }), stand: "working" },
      { a: agent({ id: "child", parent: "owner", name: "Child approval" }), stand: "needs" },
    ]);
    assert.equal(rows.length, 2, status);
    assert.deepEqual(rows.filter((r) => matches("browser audit", {}, r)), []);
    assert.deepEqual(rows.filter((r) => matches("", { state: ["needs"] }, r)), []);
    assert.deepEqual(names(rows.filter((r) => matches("", { state: ["working"] }, r))), ["Middle task"]);
    assert.deepEqual(names(sortRows(rows, "newest")), ["Middle task", "Release manager"]);
    assert.deepEqual(rows.map((r) => r.a.id), ["owner", "peer"]);
  }
});

test("the list preserves agent and child metadata used by chats and isolation", () => {
  const child = task({ parent_task: "native-parent", ended: 5000 });
  const owner = agent({
    cost_usd: 42, queued: 3, tasks_running: 8, files_changed: 10, lines_added: 200, lines_removed: 40,
    plan_run: "plan", plan: "paid", account: "owner@example.test", subagents: [child],
    harness_session: "real-harness", goal: "Release", todos_total: 5,
    workspace: { kind: "slates", over: "/work" },
    approvals: [{ id: "approve", tool: "shell", tool_name: "exec", title: "Run a command", at: 4000 }],
  });
  const managed: MissionRow = { a: owner, stand: "needs" };
  const snapshot = structuredClone(owner);
  const rows = missionRows([managed]);
  assert.equal(rows.length, 1);
  assert.equal(rows[0], managed);
  assert.equal(rows[0]?.a.subagents?.[0], child);
  assert.deepEqual(owner, snapshot);
});

test("discovery cannot reintroduce managed or native child sessions", () => {
  const session = (id: string, managed_by?: string): AiSession => ({
    id, managed_by, machine: "fedora", tool: "codex", pid: 1, name: id, title: id, cwd: "/work",
    branch: "main", model: "model", started_at: 1, last_activity: 2, state: "idle", last_prompt: "",
    last_reply: "", terminal: null, transcript: true,
  });
  const all = [
    { a: agent({ harness_session: "owner-session", subagents: [task()] }) },
    { a: agent({ id: "child", parent: "owner", harness_session: "child-session" }) },
  ];
  const found = [session("owner-session"), session("child-session"), session("native-child"), session("other-managed", "other"), session("external")];
  assert.deepEqual(missionFoundSessions(found, all).map((s) => s.id), ["external"]);
});

test("Mission Control conversation search excludes child agents and native thread events", () => {
  const hit = (id: string, agent: string, parent?: string): SaidHit => ({
    agent, name: agent, machine: "fedora",
    event: { id, seq: 1, at: 1, kind: "assistant", text: "Release findings", parent },
  });
  const hits = [hit("root", "owner"), hit("native", "owner", "native-child"), hit("managed", "child"), hit("unknown", "missing"), hit("ended", "ended-root")];
  assert.deepEqual(missionSaid(hits, new Set(["owner", "ended-root"])).map((h) => h.event.id), ["root", "ended"]);
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
