import assert from "node:assert/strict";
import { registerHooks } from "node:module";
import { test } from "node:test";
import type { AgentInfo, SaidHit, SubagentBrief } from "../src/api/harness.ts";
import type { AiSession } from "../src/api/types.ts";
import { missionFoundSessions, missionSaid, type MissionRow } from "../src/views/harness/mission-rows.ts";

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

test("an agent started through kiln's API is a top-level row; a subagent never is", () => {
  const states = ["running", "pending", "waiting", "paused", "completed", "failed", "stopped", "killed", "cancelled", "disconnected", "idle"];
  for (const status of states) {
    // The roster as Mission Control lists it: every agent kiln runs, whoever started it.
    const rows: MissionRow[] = [
      { a: agent({ status: "exited", subagents: [task({ status })] }), stand: "ended" },
      { a: agent({ id: "peer", name: "Middle task", created_at: 2000 }), stand: "working" },
      { a: agent({ id: "started", parent: "owner", name: "Started by an agent", created_at: 3000 }), stand: "needs" },
    ];
    assert.deepEqual(rows.map((r) => r.a.id), ["owner", "peer", "started"], status);
    // The subagent is no row, no count and no facet: its words find nothing.
    assert.deepEqual(rows.filter((r) => matches("browser audit", {}, r)), []);
    // The agent another agent started counts, filters and sorts like any other.
    assert.deepEqual(names(rows.filter((r) => matches("", { state: ["needs"] }, r))), ["Started by an agent"]);
    assert.deepEqual(names(rows.filter((r) => matches("started by", {}, r))), ["Started by an agent"]);
    assert.deepEqual(names(rows.filter((r) => matches("", { state: ["working"] }, r))), ["Middle task"]);
    assert.deepEqual(names(sortRows(rows, "newest")), ["Started by an agent", "Middle task", "Release manager"]);
  }
});

test("sessions found on the machines never list kiln's own agents or their subagents again", () => {
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

test("conversation search covers agents started through kiln's API, but not subagent thread events", () => {
  const hit = (id: string, agent: string, parent?: string): SaidHit => ({
    agent, name: agent, machine: "fedora",
    event: { id, seq: 1, at: 1, kind: "assistant", text: "Release findings", parent },
  });
  const hits = [hit("root", "owner"), hit("native", "owner", "native-child"), hit("managed", "child"), hit("unknown", "missing"), hit("ended", "ended-root")];
  assert.deepEqual(missionSaid(hits, new Set(["owner", "child", "ended-root"])).map((h) => h.event.id), ["root", "managed", "ended"]);
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
  assert.deepEqual(names(sortRows(rows, "priority")), ["Delta", "Gamma", "Beta", "Alpha", "Epsilon"]);
});

test("needs-first puts working and ready before finished, followed by every inactive state", () => {
  const states = ["ended", "offline", "paused", "crashed", "finished", "ready", "working", "needs"] as const;
  const rows: MissionRow[] = states.map((stand, i) => ({
    a: agent({ id: stand, name: stand, created_at: 9000 - i * 1000, last_activity: 9000 - i * 1000 }),
    stand,
  }));
  assert.deepEqual(sortRows(rows, "priority").map((r) => r.stand), ["needs", "working", "ready", "finished", "crashed", "paused", "offline", "ended"]);
  assert.deepEqual(rows.map((r) => r.stand), states, "sorting does not mutate the live roster");
});

test("live lifecycle changes update priority even when finished agents are newer", () => {
  const rows: MissionRow[] = [
    { a: agent({ id: "work", name: "Work", created_at: 1000 }), stand: "working" },
    { a: agent({ id: "ready", name: "Ready", created_at: 2000 }), stand: "ready" },
    { a: agent({ id: "done", name: "Done", created_at: 3000, last_activity: 10000 }), stand: "finished" },
  ];
  assert.deepEqual(names(sortRows(rows, "priority")), ["Work", "Ready", "Done"]);
  const updated = rows.map((r): MissionRow => r.a.id === "done" ? { ...r, stand: "working" } : r.a.id === "work" ? { ...r, stand: "finished" } : r);
  assert.deepEqual(names(sortRows(updated, "priority")), ["Done", "Ready", "Work"]);
  const needs = updated.map((r): MissionRow => r.a.id === "work" ? { ...r, stand: "needs" } : r);
  assert.deepEqual(names(sortRows(needs, "priority")), ["Work", "Done", "Ready"]);
});
