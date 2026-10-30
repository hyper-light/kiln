import assert from "node:assert/strict";
import { test } from "node:test";
import type { AgentInfo, SubagentBrief, Task } from "../src/api/harness.ts";
import { standOf, subagentsLine } from "../src/views/harness/mission-rows.ts";
import {
  askWords,
  classOf,
  isSubagent,
  stopperOf,
  subagentAsks,
  subagentPlaceholder,
  subagentRunning,
  subagentsAtWork,
  subagentTasksAtWork,
  takesMessages,
  throughSubagents,
  workingThrough,
} from "../src/views/harness/tasks.ts";

const sub = (status: string, rest: Partial<SubagentBrief> = {}): SubagentBrief => ({ id: `sub-${status}`, description: `Audit (${status})`, status, started: 1000, ...rest });
const task = (rest: Partial<Task>): Task => ({ id: "task", description: "Task", status: "running", started: 1000, ...rest });
const agent = (rest: Partial<AgentInfo> = {}): AgentInfo => ({
  id: "owner",
  name: "Release manager",
  title: "Release work",
  kind: "claude",
  kind_name: "Claude Code",
  target: "agent:fedora",
  cwd: "/work/kiln",
  model: "model",
  mode: "default",
  created_at: 1000,
  last_activity: 9000,
  status: "idle",
  approvals: [],
  queued: 0,
  tasks_running: 0,
  cost_usd: 0,
  ...rest,
});

test("its turn over, a subagent still at work keeps an agent working until its subagents end", () => {
  for (const status of ["running", "pending", "paused"]) {
    const a = agent({ subagents: [sub("completed"), sub(status)] });
    assert.equal(throughSubagents(a), true, status);
    assert.equal(standOf(a, 0), "working", status);
    assert.equal(standOf(a, 9000), "working", `${status}, seen`);
  }
  for (const status of ["completed", "failed", "stopped", "killed", "cancelled"]) {
    const a = agent({ subagents: [sub(status)] });
    assert.equal(throughSubagents(a), false, status);
    assert.equal(standOf(a, 0), "finished", status);
    assert.equal(standOf(a, 9000), "ready", `${status}, seen`);
  }
  // Older servers send no subagents.
  assert.equal(throughSubagents(agent()), false);
  assert.equal(standOf(agent(), 0), "finished");
});

test("only subagents count: shells, monitors, workflows and calls left running don't keep it working", () => {
  const left = [
    task({ id: "shell", class: "shell" }),
    task({ id: "monitor", class: "monitor" }),
    task({ id: "workflow", class: "workflow" }),
    task({ id: "mcp", class: "mcp" }),
    task({ id: "old-shell", kind: "local_bash" }),
    task({ id: "old-monitor", kind: "monitor_mcp" }),
  ];
  assert.deepEqual(subagentTasksAtWork(left), []);
  assert.equal(workingThrough("idle", subagentTasksAtWork(left).length), false);
  const tasks = [...left, task({ id: "child", class: "agent" }), task({ id: "old-child", kind: "local_agent", status: "pending" }), task({ id: "done", class: "agent", status: "completed" })];
  assert.deepEqual(
    subagentTasksAtWork(tasks).map((t) => t.id),
    ["child", "old-child"],
  );
  assert.equal(workingThrough("idle", subagentTasksAtWork(tasks).length), true);
  // What it left running shows on its entry, but says nothing about subagents.
  assert.equal(standOf(agent({ left_behind: 3, tasks_running: 3 }), 0), "finished");
});

test("needing you, a crash, a usage-limit pause, an offline machine and an end outrank its subagents", () => {
  const subagents = [sub("running")];
  const pause = { reason: "limit", since: 1000, auto: true, by: "harness" };
  assert.equal(standOf(agent({ subagents, status: "waiting" }), 0), "needs");
  assert.equal(standOf(agent({ subagents, approvals: [{ id: "ask", tool: "shell", tool_name: "Bash", title: "rm -rf target", at: 1 }] }), 0), "needs");
  assert.equal(standOf(agent({ subagents, pause }), 0), "paused");
  assert.equal(standOf(agent({ subagents, status: "disconnected" }), 0), "offline");
  assert.equal(standOf(agent({ subagents, status: "exited" }), 0), "ended");
  assert.equal(standOf(agent({ subagents, status: "exited", exit: { code: 1, error: "it crashed" } }), 0), "crashed");
  // Its own turn under way: working anyway, not through its subagents.
  assert.equal(standOf(agent({ subagents, status: "running" }), 0), "working");
  assert.equal(throughSubagents(agent({ subagents, status: "running" })), false);
});

test("the shimmer marks a subagent running, not one waiting to start or paused", () => {
  assert.equal(subagentRunning([sub("pending"), sub("paused"), sub("completed")]), false);
  assert.equal(subagentRunning([sub("completed"), sub("running")]), true);
  assert.equal(subagentRunning(undefined), false);
});

test("its line says how many subagents work and what each does", () => {
  assert.equal(subagentsLine([sub("running", { description: "System page figures" })]), "1 subagent · System page figures");
  const subagents = [sub("running", { description: "Audit" }), sub("completed", { description: "Done" }), sub("pending", { description: "Bench" })];
  assert.equal(subagentsLine(subagentsAtWork(subagents)), "2 subagents · Audit · Bench");
});

test("kiln stops what it took over on any harness; otherwise the task says whether its harness can", () => {
  assert.equal(stopperOf({ id: "left-4242", stoppable: false }, []), "kiln");
  // A Codex terminal: Codex doesn't say stop_task, but it can stop this one.
  assert.equal(stopperOf({ id: "terminal-7", stoppable: true }, []), "harness");
  // A Codex subagent: it can't.
  assert.equal(stopperOf({ id: "child", stoppable: false }, ["stop_task"]), null);
  // A server from before `stoppable`: whether the harness stops tasks at all.
  assert.equal(stopperOf({ id: "old" }, ["stop_task"]), "harness");
  assert.equal(stopperOf({ id: "old" }, []), null);
});

test("tasks from machines before `class` are named by the harness's own task type", () => {
  assert.equal(classOf({ kind: "local_bash" }), "shell");
  assert.equal(classOf({ kind: "monitor_mcp" }), "monitor");
  assert.equal(classOf({ kind: "local_workflow" }), "workflow");
  assert.equal(classOf({ kind: "remote_mcp_call" }), "mcp");
  assert.equal(classOf({ kind: "local_agent" }), "agent");
  assert.equal(classOf({ class: "shell", kind: "local_agent" }), "shell");
  assert.equal(isSubagent({}), true);
  assert.equal(isSubagent({ class: "workflow" }), false);
});

test("over a subagent's thread the composer writes to it only while it takes messages, and says how they reach it", () => {
  assert.equal(takesMessages({ takes: ["queue", "steer", "now"] }), true);
  assert.equal(takesMessages({ takes: [] }), false);
  assert.equal(takesMessages({}), false);
  assert.equal(takesMessages(undefined), false);
  assert.equal(subagentPlaceholder(null, false), "Message the subagent");
  assert.equal(subagentPlaceholder("steer", false), "Steer the subagent — it reads this at its next step");
  assert.equal(subagentPlaceholder("now", false), "Stop the subagent and say…");
  assert.equal(subagentPlaceholder("queue", true), "Queue a message for when the subagent finishes (the main agent passes it on)");
});

test("its subagents that wait on you, longest waiting first: those kiln started, and its harness's own by the requests that name them", () => {
  const q = { id: "q", tool: "question" as const, tool_name: "AskUserQuestion", title: "Which goes first?", at: 300 };
  const step = agent({ id: "s2", name: "Step 2: Exec through a seed", parent: "owner", subagent: true, status: "waiting", approvals: [q] });
  const idle = agent({ id: "s3", name: "Step 3: UI", parent: "owner", subagent: true });
  const shell = (id: string, agentId: string | undefined, at: number) => ({ id, tool: "shell" as const, tool_name: "Bash", title: "npm test", at, agent: agentId });
  const tasks = [
    task({ id: "a37155", tool_id: "toolu_figs", description: "Check every figure", class: "agent" }),
    task({ id: "019a-child", tool_id: "call_child", description: "Read the backoff code", class: "agent" }),
    task({ id: "b7x4k2", description: "Serve the mock", class: "shell" }),
  ];
  const asks = subagentAsks(
    "owner",
    [shell("p-own", undefined, 50), shell("p-claude", "a37155", 200), shell("p-claude-2", "a37155", 250), shell("p-codex", "Read the backoff code", 100), shell("p-gone", "nobody", 10)],
    tasks,
    [step, idle],
    new Map(),
  );
  // The agent's own request isn't a subagent's; one names a subagent once (its first); one naming none it has isn't shown.
  assert.deepEqual(
    asks.map((a) => [a.ap.id, a.name, a.who, a.thread]),
    [
      ["p-codex", "Read the backoff code", "owner", "call_child"],
      ["p-claude", "Check every figure", "owner", "toolu_figs"],
      ["q", "Step 2: Exec through a seed", "s2", "s2"],
    ],
  );
  // Answered here, it no longer waits.
  assert.deepEqual(subagentAsks("owner", [], tasks, [step], new Map([["q", 1]])), []);
  assert.equal(askWords(q), "Asks: Which goes first?");
  assert.equal(askWords({ tool: "plan", tool_name: "ExitPlanMode", title: "Plan" }), "Proposes a plan");
  assert.equal(askWords({ tool: "shell", tool_name: "Bash", title: "npm test" }), "Bash: npm test");
});
