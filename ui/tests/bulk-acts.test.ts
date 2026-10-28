import assert from "node:assert/strict";
import { test } from "node:test";
import type { AgentInfo, GoalItem, GoalStack, KindInfo } from "../src/api/harness.ts";
import type { ReachRunner } from "../src/api/types.ts";
import { actsOf, outcomeLine, placesFor, resumable, startedBy, topmost, whyNot, type Outcome } from "../src/views/harness/bulk-acts.ts";

const agent = (id: string, rest: Partial<AgentInfo> = {}): AgentInfo => ({
  id,
  name: id,
  title: "",
  kind: "claude",
  kind_name: "Claude Code",
  target: "agent:fedora",
  cwd: "/work/kiln",
  model: "model",
  mode: "default",
  created_at: 1000,
  last_activity: 2000,
  status: "idle",
  approvals: [],
  queued: 0,
  tasks_running: 0,
  cost_usd: 0,
  goal: "",
  summary: "",
  todos_done: 0,
  todos_total: 0,
  files_changed: 0,
  lines_added: 0,
  lines_removed: 0,
  ...rest,
});
const ids = (as: AgentInfo[]) => as.map((a) => a.id);
const goal = (status: GoalItem["status"]): GoalItem => ({ id: `g-${status}`, text: status, status });
const stack = (agent: string, state: GoalStack["state"], items: GoalItem[]): GoalStack => ({ agent, state, items, updated_at: 1 });
const limit = { reason: "limit", label: "Session", since: 1, auto: true, by: "account" };

test("each action takes the selected agents one agent's own would: its row's buttons and its menu", () => {
  const agents = [
    agent("working", { status: "running" }),
    agent("asks", { status: "waiting" }),
    agent("at-limit", { pause: limit }),
    // Its turn still under way: it pauses once that ends, and its row says working until then.
    agent("limit-mid-turn", { status: "running", pause: limit }),
    agent("limit-away", { status: "disconnected", pause: limit }),
    agent("crashed", { status: "exited", exit: { code: 1, error: "boom" } }),
    agent("on-server", { target: "local", status: "idle", harness_session: "s1" }),
    agent("never-started", { target: "local", status: "exited" }),
    agent("held", { held: { why: "Paused by the agent: plan ready", plan: "pl_1", since: 1 } }),
  ];
  const acts = actsOf(agents);
  assert.deepEqual(ids(acts.working), ["working", "asks", "limit-mid-turn"]);
  assert.deepEqual(ids(acts.atLimit), ["at-limit"]);
  assert.deepEqual(ids(acts.exited), ["crashed", "never-started"]);
  assert.deepEqual(ids(acts.live), ["working", "asks", "at-limit", "limit-mid-turn", "limit-away", "on-server", "held"]);
  // The kiln server's agents stay there once started, and one that ended goes nowhere.
  assert.deepEqual(ids(acts.movable), ["working", "asks", "at-limit", "limit-mid-turn", "limit-away", "held"]);
  assert.deepEqual(ids(acts.onRunner), ["working", "asks", "at-limit", "limit-mid-turn", "limit-away", "crashed", "held"]);
  // Before any goal stack is read, goals it paused for you are the only goals known paused, and none running.
  assert.deepEqual(ids(acts.goalsPaused), ["held"]);
  assert.deepEqual(ids(acts.goalsRunning), []);
  assert.deepEqual(ids(resumable(acts)), ["at-limit", "held"]);
});

test("Pause takes running goal stacks; Resume takes stacks paused part-way and those at a usage limit, each once", () => {
  const agents = [
    agent("running"),
    agent("paused-mid", { pause: limit }),
    agent("paused-after-some"),
    agent("never-started"),
    agent("nothing-to-go"),
    agent("done"),
    agent("unread"),
    agent("not-read-yet"),
  ];
  const stacks = new Map<string, GoalStack | null>([
    ["running", stack("running", "running", [goal("active"), goal("pending")])],
    ["paused-mid", stack("paused-mid", "paused", [goal("active")])],
    ["paused-after-some", stack("paused-after-some", "paused", [goal("met"), goal("pending")])],
    // Lined up but never started: Start, on its agent's page; not a resume.
    ["never-started", stack("never-started", "paused", [goal("pending")])],
    // What the server answers for an agent with no stack.
    ["nothing-to-go", stack("nothing-to-go", "paused", [])],
    ["done", stack("done", "done", [goal("met")])],
    ["unread", null],
  ]);
  const acts = actsOf(agents, stacks);
  assert.deepEqual(ids(acts.goalsRunning), ["running"]);
  assert.deepEqual(ids(acts.goalsPaused), ["paused-mid", "paused-after-some"]);
  assert.deepEqual(ids(acts.atLimit), ["paused-mid"]);
  assert.deepEqual(ids(resumable(acts)), ["paused-mid", "paused-after-some"], "at a limit with its goals paused too: once");
});

const runner = (name: string, rest: Partial<ReachRunner> = {}): ReachRunner => ({
  name,
  hostname: name,
  os: "linux",
  arch: "x86_64",
  labels: [],
  connected: true,
  reach: true,
  fingerprint: `fp-${name}`,
  cpus: 8,
  cpu_pct: 0,
  mem_total: 1,
  mem_used: 0,
  harnesses: [],
  ...rest,
});
const kind = (id: string, name: string, available = true): KindInfo => ({ id, name, verified: true, available });
const claude = kind("claude", "Claude Code");
const codex = kind("codex", "Codex");

test("the machines the agents can move to: each that takes some of them, which, and why the rest can't go", () => {
  const runners = [
    runner("fedora"),
    runner("mac", { os: "macos", arch: "aarch64" }),
    runner("pi"),
    runner("old", { fingerprint: null, reach: false }),
    runner("closed", { reach: false }),
    runner("away", { connected: false }),
    runner("ancient", { fingerprint: null, reach: false }),
  ];
  const byTarget: Record<string, KindInfo[]> = {
    "agent:fedora": [claude, codex],
    "agent:mac": [claude],
    "agent:pi": [codex, kind("claude", "Claude Code", false)],
    "agent:old": [claude],
    "agent:closed": [claude, codex],
    "agent:away": [claude, codex],
  };
  const onFedora = agent("on-fedora");
  const codexOnMac = agent("codex-on-mac", { target: "agent:mac", kind: "codex", kind_name: "Codex" });
  const onAncient = agent("on-ancient", { target: "agent:ancient" });
  const ended = agent("ended", { target: "agent:mac", status: "exited" });
  const onServer = agent("on-server", { target: "local", harness_session: "s1" });
  const plan = placesFor([onFedora, codexOnMac, onAncient, ended, onServer], byTarget, runners);
  assert.deepEqual(
    plan.places.map((p) => [p.name, p.platform, ids(p.takes)]),
    [
      ["fedora", "linux/x86_64", ["codex-on-mac"]],
      ["mac", "macos/aarch64", ["on-fedora"]],
      ["pi", "linux/x86_64", ["codex-on-mac"]],
    ],
  );
  assert.deepEqual(plan.older, ["old"], "a connected machine with one of their harnesses, too old to take agents");
  assert.deepEqual(plan.closed, ["closed"]);
  assert.deepEqual([...plan.stuck], [["on-ancient", "ancient runs an older kiln that can't hand agents over"]]);
  const [fedora, mac] = plan.places;
  assert.ok(fedora && mac);
  assert.equal(whyNot(codexOnMac, fedora, plan), undefined, "it goes");
  assert.equal(whyNot(onFedora, fedora, plan), "it's on fedora already");
  assert.equal(whyNot(codexOnMac, mac, plan), "it's on mac already");
  assert.equal(whyNot(onAncient, mac, plan), "ancient runs an older kiln that can't hand agents over");
  const piOnly = placesFor([agent("pi-claude", { target: "agent:fedora" })], { "agent:pi": [codex] }, [runner("fedora"), runner("pi")]);
  assert.deepEqual(piOnly.places, []);
  assert.deepEqual([...piOnly.stuck.values()], ["no other connected machine that takes agents has Claude Code"]);
});

test("ending agents ends the ones they started that still run, unless kept; one started by another of them ends with it", () => {
  const all = [
    agent("lead"),
    agent("child", { parent: "lead" }),
    agent("grandchild", { parent: "child" }),
    agent("ended-child", { parent: "lead", status: "exited" }),
    agent("its-child", { parent: "ended-child" }),
    agent("other"),
    agent("loop-a", { parent: "loop-b" }),
    agent("loop-b", { parent: "loop-a" }),
  ];
  // Generation by generation; one that ended isn't ended again, but what it started still runs.
  assert.deepEqual(ids(startedBy(new Set(["lead"]), all)), ["child", "grandchild", "its-child"]);
  assert.deepEqual(ids(startedBy(new Set(["lead", "child"]), all)), ["grandchild", "its-child"], "one selected isn't one they started");
  assert.deepEqual(ids(startedBy(new Set(["other"]), all)), []);
  assert.deepEqual(topmost(["grandchild", "other", "lead", "child"], all), ["other", "lead"]);
  assert.deepEqual(topmost(["loop-a"], all), ["loop-a"], "a loop of forebears ends the climb");
});

test("how it went, in a line", () => {
  const o = (result: Outcome["result"]): Outcome => ({ id: result, name: result, machine: "fedora", result });
  const resumed = (k: string) => `Resumed ${k}`;
  assert.equal(outcomeLine(resumed, [o("done")]), "Resumed 1 agent");
  assert.equal(outcomeLine(resumed, [o("done"), o("done"), o("skipped"), o("failed")]), "Resumed 2 agents · 1 skipped · 1 failed");
  assert.equal(outcomeLine(resumed, [o("failed"), o("failed")]), "2 failed");
  assert.equal(outcomeLine(resumed, []), "Nothing to do");
});
