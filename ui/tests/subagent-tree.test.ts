import assert from "node:assert/strict";
import { test } from "node:test";
import type { AgentInfo } from "../src/api/harness.ts";
import { briefsOf, keepOf, stepLive, subagentRows, subagentTree, threadCrumbs, threadHref, topOf, treePaths, within, wider, type Brief } from "../src/views/harness/subagent-tree.ts";
import type { Perms } from "../src/api/wire/Perms.ts";

const agent = (id: string, rest: Partial<AgentInfo> = {}): AgentInfo => ({
  id,
  name: id,
  title: "",
  kind: "claude",
  kind_name: "Claude Code",
  target: "agent:metal-ryzen-01",
  cwd: "/work/kiln",
  model: "model",
  mode: "default",
  created_at: 0,
  last_activity: 0,
  status: "running",
  approvals: [],
  queued: 0,
  tasks_running: 0,
  cost_usd: 0,
  ...rest,
});
const brief = (id: string, started: number, rest: Partial<Brief> = {}): Brief => ({ id, description: id, status: "running", started, ...rest });
const kiln = (id: string, parent: string, created_at: number, rest: Partial<AgentInfo> = {}) => agent(id, { subagent: true, parent, created_at, ...rest });

// The Replication lead and what it runs: kiln's subagents for its plan's steps, their harness's own
// subagents under them, a kiln subagent of a kiln subagent, and one of the lead's harness's own.
const LEAD = agent("lead", { subagents: [brief("h0", 40, { status: "failed", summary: "Couldn't reach the key server" })] });
const LIST = [
  LEAD,
  kiln("k1", "lead", 10, {
    name: "Leader election",
    subagents: [brief("h1a", 11, { summary: "Reading src/raft/vote.rs" }), brief("h1b", 12, { status: "completed", summary: "Elections settle within two terms." })],
  }),
  kiln("k2", "lead", 20, {
    name: "Follower catch-up",
    target: "agent:metal-epyc-01",
    kind_name: "Codex",
    approvals: [{ id: "ap", tool: "shell", tool_name: "Bash", title: "cargo test catch_up", at: 5 }],
  }),
  kiln("k3", "lead", 30, { name: "Snapshot transfer", doing: "Streaming a 10 GB snapshot" }),
  kiln("k3a", "k3", 31, { name: "Verify the checksum", subagents: [brief("h3a", 32, { last_tool: "Read" })] }),
  agent("elsewhere"),
];

test("the tree: kiln's subagents and their harness's own together, under each in the order they started, at any depth", () => {
  const nodes = subagentTree(LEAD, LIST);
  assert.deepEqual(
    nodes.map((n) => [n.key, n.kind, n.depth, n.parent]),
    [
      ["k1", "kiln", 0, null],
      ["k1/h1a", "harness", 1, "k1"],
      ["k1/h1b", "harness", 1, "k1"],
      ["k2", "kiln", 0, null],
      ["k3", "kiln", 0, null],
      ["k3a", "kiln", 1, "k3"],
      ["k3a/h3a", "harness", 2, "k3a"],
      ["lead/h0", "harness", 0, null],
    ],
  );
});

test("each says where it stands and what it's at, in Mission control's words for a request", () => {
  const by = new Map(subagentTree(LEAD, LIST).map((n) => [n.key, n]));
  assert.deepEqual([by.get("k1/h1a")?.stand, by.get("k1/h1a")?.line], ["working", "Reading src/raft/vote.rs"]);
  assert.deepEqual([by.get("k1/h1b")?.stand, by.get("k1/h1b")?.line], ["done", "Elections settle within two terms."]);
  assert.deepEqual([by.get("k2")?.stand, by.get("k2")?.line, by.get("k2")?.mono, by.get("k2")?.machine, by.get("k2")?.harness], ["needs", "Bash: cargo test catch_up", true, "metal-epyc-01", "Codex"]);
  assert.deepEqual([by.get("k3")?.stand, by.get("k3")?.line, by.get("k3")?.mono], ["working", "Streaming a 10 GB snapshot", false]);
  assert.deepEqual([by.get("k3a/h3a")?.line], ["Read"]);
  assert.deepEqual([by.get("lead/h0")?.stand, by.get("lead/h0")?.line], ["failed", "Couldn't reach the key server"]);
});

test("from the list alone (no summaries there), a harness's own subagent says what it's doing and, done, nothing it reported", () => {
  const a = agent("l", { subagents: [brief("w", 1, { last_tool: "Read" }), brief("d", 2, { status: "completed" })] });
  assert.deepEqual(
    subagentTree(a, [a]).map((n) => [n.stand, n.line]),
    [
      ["working", "Read"],
      ["done", ""],
    ],
  );
});

test("its thread opens on its agent's page: the lead's for its own, a kiln subagent's own page for that one's", () => {
  const by = new Map(subagentTree(LEAD, LIST).map((n) => [n.key, n.href]));
  assert.equal(by.get("k1"), "/agents/lead?task=k1");
  assert.equal(by.get("lead/h0"), "/agents/lead?task=h0");
  assert.equal(by.get("k1/h1a"), "/agents/k1?own=1&task=h1a");
  assert.equal(by.get("k3a"), "/agents/k3?own=1&task=k3a");
  assert.equal(by.get("k3a/h3a"), "/agents/k3a?own=1&task=h3a");
  assert.equal(threadHref("a", "a", "x y"), "/agents/a?task=x%20y");
});

test("a harness's own subagent that a request names needs you, in the request's words", () => {
  const a = agent("solo", {
    subagents: [brief("t1", 1, { tool_id: "toolu_1" }), brief("t2", 2)],
    approvals: [{ id: "p", tool: "shell", tool_name: "Bash", title: "rm -rf build", at: 1, agent: "toolu_1" }],
  });
  const nodes = subagentTree(a, [a]);
  assert.deepEqual(
    nodes.map((n) => [n.key, n.stand, n.line]),
    [
      ["solo/t1", "needs", "Bash: rm -rf build"],
      ["solo/t2", "working", ""],
    ],
  );
});

test("nested by parent_task: a harness's own subagent's own, and one whose parent isn't there hangs under the agent", () => {
  const a = agent("n", { subagents: [brief("p", 1), brief("c", 2, { parent_task: "p" }), brief("orphan", 3, { parent_task: "gone" })] });
  assert.deepEqual(
    subagentTree(a, [a]).map((n) => [n.key, n.depth]),
    [
      ["n/p", 0],
      ["n/c", 1],
      ["n/orphan", 0],
    ],
  );
});

test("the rows a list's merged briefs make (Mission control's rows) don't show kiln's subagents twice", () => {
  const merged = { ...LEAD, subagents: [...(LEAD.subagents ?? []), { id: "k1", description: "Leader election", status: "running", started: 10 }] };
  assert.equal(subagentTree(merged, LIST).filter((n) => n.key.endsWith("k1")).length, 1);
});

test("each row's piece of the path: lines passing by for ancestors with a next, a curve into a first child, a line on to its next or its own", () => {
  const rows = subagentRows(LEAD, LIST);
  const shape = Object.fromEntries(
    rows.map((r) => [
      r.key,
      {
        lane: r.path.lane,
        pieces: r.path.pieces.map((p) => `${p.lane}:${p.from}-${p.to}${p.hot ? "!" : ""}`),
        curve: r.path.curve ? `${r.path.curve.from}>${r.path.curve.to}${r.path.curve.hot ? "!" : ""}` : null,
      },
    ]),
  );
  assert.deepEqual(shape, {
    k1: { lane: 0, pieces: ["0:top-mark", "0:mark-bottom"], curve: null },
    "k1/h1a": { lane: 1, pieces: ["0:top-bottom", "1:mark-bottom"], curve: "0>1" },
    "k1/h1b": { lane: 1, pieces: ["0:top-bottom", "1:top-mark"], curve: null },
    // Needs you: its own piece is tan, the lines passing by aren't.
    k2: { lane: 0, pieces: ["0:top-mark!", "0:mark-bottom!"], curve: null },
    k3: { lane: 0, pieces: ["0:top-mark", "0:mark-bottom"], curve: null },
    k3a: { lane: 1, pieces: ["0:top-bottom", "1:mark-bottom"], curve: "0>1" },
    "k3a/h3a": { lane: 2, pieces: ["0:top-bottom"], curve: "1>2" },
    "lead/h0": { lane: 0, pieces: ["0:top-mark"], curve: null },
  });
});

test("a first child that needs you: its curve in is tan; a last one's line in is, and nothing hangs on after it", () => {
  const a = agent("r", {
    subagents: [brief("p", 1), brief("c", 2, { parent_task: "p", tool_id: "tc" })],
    approvals: [{ id: "q", tool: "question", tool_name: "AskUserQuestion", title: "Which first?", at: 1, agent: "tc" }],
  });
  const paths = treePaths(subagentTree(a, [a]));
  assert.deepEqual(paths[1], { lane: 1, pieces: [], curve: { from: 0, to: 1, hot: true } });
  assert.deepEqual(paths[0], {
    lane: 0,
    pieces: [
      { lane: 0, from: "top", to: "mark", hot: false },
      { lane: 0, from: "mark", to: "bottom", hot: false },
    ],
    curve: null,
  });
});

test("a live state's tasks as the list has them: only subagents, with what they're at", () => {
  const b = briefsOf([
    { id: "s", description: "Audit", status: "running", started: 1, class: "agent", last_tool: "Bash", summary: "Running the tests" },
    { id: "sh", description: "npm run dev", status: "running", started: 2, class: "shell" },
  ]);
  assert.deepEqual(b, [
    {
      id: "s",
      tool_id: undefined,
      description: "Audit",
      status: "running",
      parent_task: undefined,
      started: 1,
      ended: undefined,
      last_tool: "Bash",
      pausable: undefined,
      paused_at: undefined,
      perms: undefined,
      summary: "Running the tests",
    },
  ]);
});

test("the agent at a plan's step, live: what it's doing since its turn started, or what it asks; its thread and the session that answers", () => {
  const at = agent("s1", { doing: "Running cargo test", turn_started: 500 });
  assert.deepEqual(stepLive(at, "/agents/lead?task=s1"), { line: "Running cargo test", ask: undefined, started: 500, href: "/agents/lead?task=s1", session: "s1" });
  const ap = { id: "ap", tool: "shell" as const, tool_name: "Bash", title: "cargo test catch_up", at: 5 };
  const asking = agent("s2", { status: "waiting", approvals: [ap] });
  assert.deepEqual(stepLive(asking, "/agents/lead?task=s2"), { line: "Bash: cargo test catch_up", ask: ap, started: 0, href: "/agents/lead?task=s2", session: "s2" });
});

test("a thread's crumbs: the agent at the top, each subagent it's under, then it, each where its thread opens", () => {
  assert.equal(topOf(LIST, "k3a")?.id, "lead");
  assert.equal(topOf(LIST, "lead")?.id, "lead");
  assert.equal(topOf(LIST, "nobody"), undefined);
  const nodes = subagentTree(LEAD, LIST);
  assert.deepEqual(
    threadCrumbs(LEAD, nodes, "k3a/h3a")?.map((c) => [c.name, c.href]),
    [
      ["lead", "/agents/lead"],
      ["Snapshot transfer", "/agents/lead?task=k3"],
      ["Verify the checksum", "/agents/k3?own=1&task=k3a"],
      ["h3a", "/agents/k3a?own=1&task=h3a"],
    ],
  );
  assert.deepEqual(
    threadCrumbs(LEAD, nodes, "lead/h0")?.map((c) => c.name),
    ["lead", "h0"],
  );
  assert.equal(threadCrumbs(LEAD, nodes, "lead/gone"), null);
});

test("one whose parent kiln no longer lists heads its own line, and parents that name each other end", () => {
  assert.equal(topOf([kiln("o", "gone", 1)], "o")?.id, "o");
  const loop = [kiln("a", "b", 1), kiln("b", "a", 2)];
  assert.ok(["a", "b"].includes(topOf(loop, "a")?.id ?? ""));
});

test("a person's pause: its own says Paused and can be resumed; what's under it says Paused, held by the pause over it", () => {
  const lead = agent("pl");
  const list = [
    lead,
    kiln("pk", "pl", 10, { frozen: 99, subagents: [brief("ph", 11, { pausable: true }), brief("ph-done", 12, { status: "completed", pausable: false })] }),
    kiln("pk2", "pk", 13),
    kiln("pk-off", "pl", 20, { status: "exited" }),
    agent("own", { parent: "pl", subagent: true, created_at: 30, subagents: [brief("oh", 31, { pausable: true, paused_at: 50 }), brief("oh-under", 32, { pausable: true, parent_task: "oh" })] }),
  ];
  const by = new Map(subagentTree(lead, list).map((n) => [n.key, [n.stand, n.paused, n.pausable]]));
  assert.deepEqual(by.get("pk"), ["paused", "own", true]);
  assert.deepEqual(by.get("pk/ph"), ["paused", "above", false], "what runs in a paused session is paused with it");
  assert.deepEqual(by.get("pk/ph-done"), ["done", "above", false], "one that ended stays as it was");
  assert.deepEqual(by.get("pk2"), ["paused", "above", false]);
  assert.deepEqual(by.get("pk-off"), ["done", undefined, false], "ended: nothing to pause");
  assert.deepEqual(by.get("own/oh"), ["paused", "own", true]);
  assert.deepEqual(by.get("own/oh-under"), ["paused", "above", false]);
  // Live and not paused: Pause.
  const free = subagentTree(lead, [lead, kiln("f", "pl", 1)]).find((n) => n.key === "f");
  assert.deepEqual([free?.stand, free?.paused, free?.pausable], ["working", undefined, true]);
});

test("what a subagent may do: never more than what it's under (model.rs Perms::narrow), and each node knows what's over it", () => {
  const up: Perms = { read: "allow", edit: "ask", run: "off", web: "ask", folder: "/w/kiln/", minutes: 30 };
  const asked: Perms = { read: "ask", edit: "allow", run: "allow", web: "off", folder: "/w/kiln/crates/raft", minutes: 90, tokens: 50_000 };
  assert.deepEqual(within(asked, up), { read: "ask", edit: "ask", run: "off", web: "off", folder: "/w/kiln/crates/raft", minutes: 30, tokens: 50_000 });
  assert.equal(within({ ...asked, folder: "/w/kiln-other" }, up)?.folder, "/w/kiln", "beside its parent's folder, not in it: its parent's");
  assert.equal(within(undefined, undefined), undefined);
  assert.deepEqual(within(undefined, up), up);
  assert.ok(wider("allow", "ask") && !wider("ask", "ask") && !wider("allow", undefined));
  const lead = agent("pl", { perms: { read: "allow", edit: "allow", run: "ask", web: "allow" } });
  const own: Perms = { read: "allow", edit: "off", run: "allow", web: "allow" };
  const list = [lead, kiln("pk", "pl", 1, { perms: own, subagents: [brief("ph", 2)] })];
  const by = new Map(subagentTree(lead, list).map((n) => [n.key, n]));
  assert.deepEqual([by.get("pk")?.perms, by.get("pk")?.over], [own, lead.perms]);
  assert.deepEqual(
    by.get("pk/ph")?.over,
    { read: "allow", edit: "off", run: "ask", web: "allow", folder: undefined, minutes: undefined, tokens: undefined },
    "under its session: that one's, as narrowed by its agent's",
  );
});

test("Keep on a variant tells its parent to use its result, naming what it varied, its id and how to read it", () => {
  const lead = agent("kl", { subagents: [brief("kt", 1, { description: "Measure election time" })] });
  const varied = kiln("kv", "kl", 2, { name: "Follower catch-up" });
  const v = kiln("kv2", "kl", 3, { name: "Catch up in batches", variant_of: "kv" });
  const ofTask = kiln("kv3", "kl", 4, { name: "Five nodes", variant_of: "kl/kt" });
  const list = [lead, varied, v, ofTask];
  assert.deepEqual(keepOf(v, list), {
    to: "kl",
    text: "Keep “Catch up in batches”, the variant of “Follower catch-up”: use its result in place of that one's. Read it whole with agents_result (kv2).",
  });
  assert.equal(keepOf(ofTask, list)?.text.includes("the variant of “Measure election time”"), true);
  assert.equal(keepOf(varied, list), undefined, "not a variant");
  assert.equal(keepOf({ ...v, parent: undefined }, list), undefined, "no parent to tell");
});
