import assert from "node:assert/strict";
import { test } from "node:test";
import type { AgentInfo } from "../src/api/harness.ts";
import { briefsOf, subagentRows, subagentTree, threadHref, treePaths, type Brief } from "../src/views/harness/subagent-tree.ts";

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
  kiln("k1", "lead", 10, { name: "Leader election", subagents: [brief("h1a", 11, { summary: "Reading src/raft/vote.rs" }), brief("h1b", 12, { status: "completed", summary: "Elections settle within two terms." })] }),
  kiln("k2", "lead", 20, { name: "Follower catch-up", target: "agent:metal-epyc-01", kind_name: "Codex", approvals: [{ id: "ap", tool: "shell", tool_name: "Bash", title: "cargo test catch_up", at: 5 }] }),
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
  const a = agent("solo", { subagents: [brief("t1", 1, { tool_id: "toolu_1" }), brief("t2", 2)], approvals: [{ id: "p", tool: "shell", tool_name: "Bash", title: "rm -rf build", at: 1, agent: "toolu_1" }] });
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
  const a = agent("r", { subagents: [brief("p", 1), brief("c", 2, { parent_task: "p", tool_id: "tc" })], approvals: [{ id: "q", tool: "question", tool_name: "AskUserQuestion", title: "Which first?", at: 1, agent: "tc" }] });
  const paths = treePaths(subagentTree(a, [a]));
  assert.deepEqual(paths[1], { lane: 1, pieces: [], curve: { from: 0, to: 1, hot: true } });
  assert.deepEqual(paths[0], { lane: 0, pieces: [{ lane: 0, from: "top", to: "mark", hot: false }, { lane: 0, from: "mark", to: "bottom", hot: false }], curve: null });
});

test("a live state's tasks as the list has them: only subagents, with what they're at", () => {
  const b = briefsOf([
    { id: "s", description: "Audit", status: "running", started: 1, class: "agent", last_tool: "Bash", summary: "Running the tests" },
    { id: "sh", description: "npm run dev", status: "running", started: 2, class: "shell" },
  ]);
  assert.deepEqual(b, [{ id: "s", tool_id: undefined, description: "Audit", status: "running", parent_task: undefined, started: 1, ended: undefined, last_tool: "Bash", summary: "Running the tests" }]);
});
