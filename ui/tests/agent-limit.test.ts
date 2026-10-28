import assert from "node:assert/strict";
import { test } from "node:test";
import type { AgentLimitView, ServerEvent } from "../src/api/types.ts";
import { limitLine, parseLimit, settable, standing, withEvent } from "../src/views/agent-limit.ts";

const view = (v: Partial<AgentLimitView>): AgentLimitView => ({
  runner: "box",
  own: false,
  connected: true,
  runs_agents: true,
  supported: true,
  max_sessions: null,
  held: { max_sessions: 64, from: "default" },
  working: 3,
  waiting: 0,
  sessions: 41,
  default: 64,
  max: 4096,
  ...v,
});

test("a runner's agent limit says how many of its agents are at work, its most at once, and where that comes from", () => {
  assert.equal(limitLine(view({})), "3 of 41 agents at work now · at most 64 at once, kiln's default");
  assert.equal(limitLine(view({ working: 1, sessions: 1, held: { max_sessions: 100, from: "config", config: 100 } })), "1 of 1 agent at work now · at most 100 at once, from its config");
  assert.equal(limitLine(view({ max_sessions: 128, held: { max_sessions: 128, from: "server", config: 100 } })), "3 of 41 agents at work now · at most 128 at once, set here (its config says 100)");
  assert.equal(limitLine(view({ max_sessions: 128, held: { max_sessions: 128, from: "server" } })), "3 of 41 agents at work now · at most 128 at once, set here");
  assert.equal(limitLine(view({ working: 0, sessions: 0 })), "No agents now · at most 64 at once, kiln's default");
  // At its limit, those waiting for room.
  assert.equal(limitLine(view({ working: 3, waiting: 2, held: { max_sessions: 3, from: "server" }, max_sessions: 3 })), "3 of 41 agents at work now, 2 waiting for room · at most 3 at once, set here");
  // Offline: how many it has, and what's set here, if anything.
  assert.equal(limitLine(view({ connected: false, working: null, waiting: null, max_sessions: 200, held: null })), "41 agents · at most 200 at once, set here");
  assert.equal(limitLine(view({ connected: false, working: null, waiting: null, held: null })), "41 agents");
});

test("what became of what's set here: in use says nothing, the rest say why", () => {
  assert.equal(standing(view({})), "");
  assert.equal(standing(view({ max_sessions: 128, held: { max_sessions: 128, from: "server" } })), "");
  assert.equal(standing(view({ max_sessions: 128 })), "box is taking it…", "set, and not held yet");
  assert.equal(standing(view({ max_sessions: null, held: { max_sessions: 128, from: "server" } })), "box is taking it…", "taken away, and still held");
  assert.equal(standing(view({ connected: false, held: null, supported: null })), "box is offline.");
  assert.equal(standing(view({ connected: false, held: null, supported: null, max_sessions: 200 })), "box is offline: it holds 200 when it connects.");
  assert.equal(
    standing(view({ supported: false, held: null })),
    "box's kiln is too old to take a limit from here: until it's upgraded (System → Updates), it holds 64 and counts every agent it has, finished ones too.",
  );
  assert.equal(standing(view({ supported: null, held: null })), "Waiting for box to say what it holds…");
  assert.equal(standing(view({ runs_agents: false, supported: false, held: null })), "box runs no agents: its terminals are off.");
  assert.match(standing(view({ own: true })), /^The server runs this machine's agents itself/);
});

test("a limit is set from here for every runner that runs agents, and the server's own", () => {
  assert.ok(settable(view({})));
  assert.ok(settable(view({ connected: false, runs_agents: null })), "offline: it holds it when it connects");
  assert.ok(!settable(view({ runs_agents: false })));
  assert.ok(settable(view({ own: true, runs_agents: false })));
});

test("a limit typed is a whole number from 1 to the most there can be", () => {
  assert.equal(parseLimit("128", 4096), 128);
  assert.equal(parseLimit(" 7 ", 4096), 7);
  assert.equal(parseLimit("4096", 4096), 4096);
  for (const bad of ["", "0", "4097", "-3", "2.5", "lots", "1e3", "12 sessions"]) {
    assert.equal(parseLimit(bad, 4096), "A whole number of agents, from 1 to 4096.", JSON.stringify(bad));
  }
});

test("a runner's agent limit loads again for what changes it, and only that", () => {
  const v = view({});
  const ev = (e: unknown) => e as ServerEvent;
  const agent = (target: string) => ev({ type: "harness", info: { id: "a1", target } });
  assert.equal(withEvent(v, agent("agent:box")), undefined, "one of its agents changed: how many are at work may have");
  assert.equal(withEvent(v, agent("agent:other")), v, "another runner's");
  assert.equal(withEvent(v, agent("local")), v, "the server's own");
  assert.equal(withEvent(view({ own: true }), agent("local")), undefined, "the server's own, on its own runner's");
  assert.equal(withEvent(v, ev({ type: "agent_limit", runner: "box" })), undefined);
  assert.equal(withEvent(v, ev({ type: "agent_limit", runner: "other" })), v);
  assert.equal(withEvent(v, ev({ type: "agent", agent: { name: "box", connected: false } })), undefined, "it went offline");
  assert.equal(withEvent(v, ev({ type: "agent", agent: { name: "box", connected: true } })), v, "its samples, nothing more");
  assert.equal(withEvent(v, ev({ type: "harness_closed", id: "a1" })), undefined, "an agent closed: whose isn't said");
});
