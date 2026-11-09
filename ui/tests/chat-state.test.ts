// Attention plan 01a199e1b177752e84dad5dc990e6a15: a chat's list and header agree across real state changes.
import assert from "node:assert/strict";
import { test } from "node:test";
import type { AgentInfo, State } from "../src/api/harness.ts";
import { defaultInfo, defaultState } from "../mock/defaults.ts";
import { turnSignal } from "../src/views/harness/chat-state.ts";
import { standOf } from "../src/views/harness/mission-rows.ts";

const agent = (rest: Partial<AgentInfo> = {}): AgentInfo => ({
  ...defaultInfo(),
  id: "chat",
  name: "Release work",
  kind: "claude",
  kind_name: "Claude Code",
  target: "agent:runner",
  cwd: "/work",
  created_at: 1,
  last_activity: 1,
  status: "idle",
  title: "Release work",
  model: "model",
  mode: "default",
  approvals: [],
  queued: 0,
  tasks_running: 0,
  cost_usd: 0,
  ...rest,
});
const state = (rest: Partial<State> = {}): State => ({ ...defaultState(), status: "idle", cwd: "/work", title: "Release work", model: "model", mode: "default", ...rest });
const signal = (info: AgentInfo, live: State | null = null) => turnSignal(live?.status ?? info.status, "open", live, info, 0, info);
const limit = { reason: "Usage exhausted", since: 1, auto: true, by: "harness" };

test("mc_attention: queued input and room admission stay Working in the list and header until they clear", () => {
  for (const pending of [{ queued: 1 }, { room_wait: 0 }, { queued: 1, room_wait: 1 }]) {
    const info = agent(pending);
    assert.equal(standOf(info), "working");
    assert.equal(signal(info).tone, "active");
  }
  assert.equal(signal(agent({ queued: 1 })).label, "Message queued");
  assert.equal(signal(agent({ room_wait: 0 })).label, "Waiting for room");
  const clear = agent();
  assert.equal(standOf(clear), "finished");
  assert.deepEqual(signal(clear, state()), { tone: "finished", label: "Finished" });
});

test("mc_attention: fresh queue and delivery evidence prevent a premature Finished header", () => {
  const info = agent();
  const queued = state({ queue: [{ id: "q", text: "Check the gate", at: 1 }] });
  assert.deepEqual(signal(info, queued), { tone: "active", label: "Message queued" });
  for (const delivery of ["sent", "queued"] as const) assert.equal(signal(info, state({ delivery: { q: delivery } })).tone, "active", delivery);
  for (const delivery of ["completed", "cancelled"] as const) assert.equal(signal(info, state({ delivery: { q: delivery } })).tone, "finished", delivery);
  assert.equal(signal(agent({ queued: 1 }), state()).tone, "active", "the roster can have a queue update ahead of the socket");
  assert.equal(signal(info, state({ room_wait: 1 })).label, "Waiting for room");
});

test("mc_attention: a person's freeze pauses live work, and a usage pause waits for its turn to end", () => {
  for (const status of ["idle", "starting", "running"] as const) {
    const info = agent({ status, frozen: 1, queued: 1, room_wait: 1, pause: limit });
    assert.equal(standOf(info), "paused", status);
    assert.deepEqual(signal(info), { tone: "stopped", label: "Paused" }, status);
  }
  assert.equal(standOf(agent({ status: "running", pause: limit })), "working");
  assert.equal(signal(agent({ status: "running", pause: limit })).label, "Working");
  assert.deepEqual(signal(agent({ pause: limit })), { tone: "stopped", label: "Paused at a usage limit" });
  assert.equal(standOf(agent({ status: "exited", frozen: 1 })), "ended");
  assert.equal(standOf(agent({ status: "exited", frozen: 1, exit: { error: "Could not start" } })), "crashed");
  assert.equal(standOf(agent({ status: "exited", frozen: 1, exit: { again_at: 2 } })), "working");
  assert.equal(standOf(agent({ status: "disconnected", frozen: 1 })), "offline");
});

test("mc_attention: requests and goals held for the person win over pauses and queued work in both surfaces", () => {
  const base = { frozen: 1, pause: limit, queued: 1, room_wait: 1 };
  const requests: Partial<AgentInfo>[] = [
    { status: "waiting" },
    { approvals: [{ id: "ask", tool: "question", tool_name: "AskUserQuestion", title: "Which release?", at: 1 }] },
    { held: { why: "Review the plan", since: 1 } },
  ];
  for (const request of requests) {
    const info = agent({ ...base, ...request });
    assert.equal(standOf(info), "needs");
    assert.deepEqual(signal(info), { tone: "needs", label: "Waiting for you" });
  }
  const held = agent({ held: { why: "Review the plan", since: 1 } });
  assert.equal(signal(held, state()).tone, "needs", "held goals belong to the fresh roster, not State");
  assert.equal(signal(agent(), state()).tone, "finished", "a fresh roster that cleared held goals no longer needs the person");
});

test("mc_attention: a fresh State clears optional flags from an older Info instead of reviving a pause", () => {
  const old = agent({ frozen: 1, pause: limit, approvals: [{ id: "ask", tool: "question", tool_name: "AskUserQuestion", title: "Which release?", at: 1 }] });
  assert.deepEqual(signal(old, state()), { tone: "finished", label: "Finished" });
  assert.equal(signal(old).tone, "needs", "Info is the fallback only before State arrives");
  assert.deepEqual(signal(agent({ status: "exited", exit: { again_at: 2 } }), state({ status: "exited" })), { tone: "stopped", label: "Stopped" });
  const room = agent({ room_wait: 1 });
  assert.equal(signal(room, state()).tone, "finished", "omitting room_wait clears the socket's older admission wait");
  assert.equal(signal(room, state({ room_wait: null })).tone, "finished", "a null room_wait clears it too");
});

test("mc_attention: live tasks and owned work keep a quiet header Ready", () => {
  assert.equal(signal(agent(), state({ tasks: [{ id: "shell", description: "Build", class: "shell", status: "running", started: 1 }] })).tone, "ready");
  assert.equal(signal(agent(), state({ left_behind: 1 })).tone, "ready");
  assert.equal(signal(agent({ tasks_running: 1 }), state()).tone, "ready");
});
