import assert from "node:assert/strict";
import { test } from "node:test";
import type { HEvent, Task } from "../src/api/harness.ts";
import { changesetOf, editsOf, runsOf, stepStatus, stepSummary, taskDot, taskDuration, taskLive, taskOfCall, turnsOf, type ToolEvent } from "../src/views/harness/derive.ts";

const call = (id: string, tool: ToolEvent["tool"], rest: Partial<ToolEvent> = {}): ToolEvent => ({
  id, seq: 1, at: 1000, kind: "tool", tool_id: id, name: tool, tool, title: id,
  input: {}, status: "done", started: 1000, ended: 1001, ...rest,
});
const task = (rest: Partial<Task> = {}): Task => ({
  id: "child", tool_id: "spawn", description: "Audit the changes", status: "running", started: 1000, ...rest,
});
const diff = (path: string) => ({ path, hunks: [{ old_start: 1, old_lines: 1, new_start: 1, new_lines: 1, lines: ["-before", "+after"] }] });

test("child history stays out of the main command list and changes while its own turn retains them", () => {
  const mainEdit = call("main-edit", "edit", { diffs: [diff("main.ts")] });
  const childEdit = call("child-edit", "edit", { parent: "spawn", diffs: [diff("child.ts")] });
  const mainRun = call("main-run", "shell");
  const childRun = call("child-run", "shell", { parent: "spawn" });
  const events: HEvent[] = [
    { id: "request", seq: 0, at: 0, kind: "user", text: "Review the changes" },
    mainEdit, childEdit, mainRun, childRun,
    { id: "child-result", seq: 5, at: 5000, parent: "spawn", kind: "assistant", text: "Reviewed the changes." },
  ];

  assert.deepEqual(runsOf(events).map((e) => e.id), ["main-run"]);
  assert.deepEqual(changesetOf(editsOf(events)).map((f) => f.path), ["main.ts"]);
  const child = turnsOf(events, "spawn")[0]!;
  assert.equal(child.outcome?.id, "child-result");
  const childCalls = child.items.flatMap((item) => item.type === "step" ? item.calls : []);
  assert.deepEqual(changesetOf(childCalls).map((f) => f.path), ["child.ts"]);
  assert.deepEqual(childCalls.filter((e) => e.tool === "shell").map((e) => e.id), ["child-run"]);
  assert.equal(turnsOf(events)[0]?.outcome, null);
});

test("a completed spawn keeps the child's running, paused and failed states distinct", () => {
  const spawn = call("spawn", "agent", { name: "subAgentActivity", input: { kind: "started" }, task: "child" });
  const child = task();
  assert.equal(stepStatus([spawn]), "done");
  assert.equal(taskDot(taskOfCall(spawn, [child])!), "running");
  const paused = { ...child, status: "paused" };
  assert.equal(taskDot(paused), "pending");
  assert.equal(taskLive(paused), true);
  const waiting: HEvent = { id: "waiting", seq: 2, at: 2000, parent: "spawn", kind: "assistant", text: "Waiting for approval." };
  const pausedTurn = turnsOf([waiting], "spawn", taskLive(paused))[0]!;
  assert.equal(pausedTurn.outcome, null);
  assert.equal(pausedTurn.items[0]?.type, "note");
  const failed = { ...child, status: "failed", ended: 136902, duration_ms: 0 };
  assert.equal(taskLive(failed), false);
  assert.equal(taskDot(failed), "failed");
  assert.equal(taskDuration(failed), 135902);
  assert.equal(taskDot({ ...failed, status: "stopped" }), "cancelled");
  assert.equal(taskDuration({ ...failed, duration_ms: 120000 }), 120000);
});

test("activity and collaboration actions do not masquerade as new subagents", () => {
  const activity = (kind?: string) => call("activity", "agent", { name: "subAgentActivity", input: { kind } });
  assert.equal(stepSummary("agent", [activity("started")]), "Started a subagent");
  assert.equal(stepSummary("agent", [activity("completed")]), "Subagent finished");
  assert.equal(stepSummary("agent", [activity("interrupted")]), "Interrupted a subagent");
  assert.equal(stepSummary("agent", [activity("interacted")]), "Subagent interaction");
  // Old snapshots have no discriminator; they cannot establish that a new child started.
  assert.equal(stepSummary("agent", [activity()]), "Subagent activity");
  assert.equal(stepSummary("agent", [call("wait", "agent", { name: "collabAgentToolCall", input: { tool: "wait" } })]), "Waited for subagents");
  assert.equal(stepSummary("agent", [call("message", "agent", { name: "collabAgentToolCall", input: { tool: "sendMessage" } })]), "Sent a message to a subagent");
  assert.equal(stepSummary("agent", [call("task", "agent", { name: "Task" })]), "Started a subagent");
});

test("follow-up activity opens the explicitly addressed child's original thread", () => {
  const earlier = task({ id: "earlier", tool_id: "shared-call" });
  const child = task();
  const followup = call("shared-call", "agent", { task: "child" });
  assert.equal(taskOfCall(followup, [earlier, child]), child);
  assert.equal(taskOfCall(followup, [earlier, child])?.tool_id, "spawn");
  assert.equal(taskOfCall(call("spawn", "agent"), [child]), child);
  assert.equal(taskOfCall(call("list", "agent"), [child]), undefined);
});
