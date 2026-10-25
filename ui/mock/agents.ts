/**
 * Agents that run agents, in the mock (docs/agents.md): an agent on the server that started three
 * on runners, one of which asks you something, so Mission control shows a family (and the asking
 * child first, naming its parent), and ending the parent offers to end them too.
 *
 *   /agents/fam-orch   "Release prep" (Claude Code, the server), working, waiting for its agents
 *   /agents/fam-tests  "Fix the flaky retry test" (Codex, metal-ryzen-01), working
 *   /agents/fam-docs   "Update the docs" (Claude Code, metal-ryzen-02), finished
 *   /agents/fam-migr   "Check the migration" (Claude Code, metal-epyc-01), asks to run a command
 *
 * And what agents leave running (the activity line by the composer, its Stop and output):
 *
 *   /agents/sub-busy   "System page" (Claude Code), its turn over, a subagent still at work, a
 *                      shell whose output grows, and a monitor; working until they end
 *   /agents/sub-done   "Release notes" (Claude Code), its subagents done: finished
 *   /agents/sub-codex  "Scheduler audit" (Codex), a subagent kiln can't stop
 *
 * The `agents.*` routes answer as the server's do, from these and nothing else.
 */
import type http from "node:http";
import type { WebSocket } from "ws";
import type { AgentInfo, Approval, HEvent, Meta, State, SubagentBrief, Task, Todo } from "../src/api/harness.ts";
import { PROPOSED } from "./pages.ts";

function json(res: http.ServerResponse, status: number, body: unknown) {
  res.writeHead(status, { "content-type": "application/json", "cache-control": "no-store" });
  res.end(JSON.stringify(body));
}
async function body<T>(req: http.IncomingMessage): Promise<T> {
  const chunks: Buffer[] = [];
  for await (const c of req) chunks.push(c as Buffer);
  const s = Buffer.concat(chunks).toString("utf8");
  return (s ? JSON.parse(s) : {}) as T;
}

let emitEvent: (ev: unknown) => void = () => {};
export function setFamilyEmit(fn: (ev: unknown) => void) {
  emitEvent = fn;
}

interface Member {
  info: AgentInfo;
  events: HEvent[];
  approval?: Approval;
  /** Its subagents and what it left running (`State.tasks`). */
  tasks?: Task[];
}

const t0 = Date.now() - 18 * 60_000;
const CWD = "/home/ada/src/kiln";

function member(id: string, kind: "claude" | "codex", target: string, name: string, status: AgentInfo["status"], mode: string, prompt: string, reply: string, extra: Partial<AgentInfo> = {}): Member {
  const at = t0 + (id === "fam-orch" ? 0 : 60_000);
  const info: AgentInfo = {
    id,
    kind,
    kind_name: kind === "claude" ? "Claude Code" : "Codex",
    target,
    name,
    cwd: CWD,
    created_at: at,
    status,
    title: name,
    model: kind === "claude" ? "claude-opus-5-5" : "gpt-5.5-codex",
    mode,
    approvals: [],
    queued: 0,
    tasks_running: 0,
    cost_usd: kind === "claude" ? 0.42 : 0.18,
    last_activity: Date.now() - 20_000,
    goal: prompt,
    summary: reply,
    ...extra,
  };
  const events: HEvent[] = [
    { id: `${id}-u1`, seq: 1, at, kind: "user", text: prompt },
    { id: `${id}-a1`, seq: 2, at: at + 40_000, kind: "assistant", text: reply },
  ];
  return { info, events };
}

const migrApproval: Approval = {
  id: "fam-migr-ap1",
  tool_name: "Bash",
  tool: "shell",
  title: "cargo run --bin migrate -- --dry-run",
  command: "cargo run --bin migrate -- --dry-run",
  input: { command: "cargo run --bin migrate -- --dry-run" },
  cwd: CWD,
  at: Date.now() - 90_000,
};

const family = new Map<string, Member>(
  [
    member(
      "fam-orch",
      "claude",
      "local",
      "Release prep",
      "running",
      "default",
      "Get 0.2.1 ready: fix the flaky retry test, bring the docs up to date and check the migration, each in an agent of its own. Then tell me what's left.",
      "Three agents are on it. Waiting for them.",
      { doing: "Waiting for its agents" },
    ),
    member(
      "fam-tests",
      "codex",
      "agent:metal-ryzen-01",
      "Fix the flaky retry test",
      "running",
      "untrusted",
      "The retry test in crates/kiln/src/http/client.rs fails about one run in ten. Find out why and fix it. Done when 200 runs pass.",
      "Running the test 200 times.",
      {
        parent: "fam-orch",
        doing: "Running cargo test -p kiln retry",
        files_changed: 1,
        lines_added: 6,
        lines_removed: 2,
        cwd: "/home/ada/.local/state/kiln/workspaces/kiln-famtests",
        workspace: { kind: "volume", over: CWD, made: { volume: "c3f2bc748ffa0cb00001000000000012", name: "kiln-famtests", mount: "/home/ada/.local/state/kiln/workspaces/kiln-famtests" } },
      },
    ),
    member(
      "fam-docs",
      "claude",
      "agent:metal-ryzen-02",
      "Update the docs",
      "idle",
      "plan",
      "List what docs/agents.md says that the code no longer does. Change nothing.",
      "Three sections are out of date: the wait cap, the stop route and the CLI's --on. Details below.",
      { parent: "fam-orch", last_activity: Date.now() - 4 * 60_000 },
    ),
    {
      ...member(
        "fam-migr",
        "claude",
        "agent:metal-epyc-01",
        "Check the migration",
        "waiting",
        "default",
        "Dry-run the 20261019 migration against a copy of the production database and report what it would change.",
        "I'll dry-run it first.",
        { parent: "fam-orch" },
      ),
      approval: migrApproval,
    },
  ].map((m) => [m.info.id, m]),
);
{
  const m = family.get("fam-migr");
  if (m) m.info.approvals = [{ id: migrApproval.id, tool: migrApproval.tool, tool_name: migrApproval.tool_name, title: migrApproval.title, at: migrApproval.at }];
}

// /agents/fan-lead "Replication lead": its goal "Replicated log" runs its plan's steps side by side
// (docs/goals.md, "A plan's steps side by side"; goals.ts), a subagent of its own for each step
// that's ready, one of them asking you something; and a goal of its own (not a plan's step) out on a
// subagent too. It coordinates them, at work itself on nothing.
const fanApproval: Approval = {
  id: "fan-s3-ap1",
  tool_name: "Bash",
  tool: "shell",
  title: "cargo run --bin chaos -- --partition 2",
  command: "cargo run --bin chaos -- --partition 2",
  input: { command: "cargo run --bin chaos -- --partition 2" },
  cwd: CWD,
  at: Date.now() - 45_000,
};
for (const m of [
  member("fan-lead", "claude", "local", "Replication lead", "idle", "default", "Replicate the metadata store's log over QUIC, then rotate the release signing key.", "Its steps are out on subagents; I'll check each as it comes back."),
  member("fan-s2", "claude", "agent:metal-ryzen-01", "Step 2: Leader election over QUIC", "running", "default", 'Step 2 of 5 of the plan "Replicated log": Leader election over QUIC', "Electing a leader across three nodes.", {
    parent: "fan-lead",
    doing: "Running cargo test -p hyper-raft election",
  }),
  { ...member("fan-s3", "codex", "agent:metal-epyc-01", "Step 3: Follower catch-up", "waiting", "untrusted", 'Step 3 of 5 of the plan "Replicated log": Follower catch-up', "I'll partition a follower and let it catch up.", { parent: "fan-lead" }), approval: fanApproval },
  member("fan-s4", "claude", "agent:metal-ryzen-02", "Step 4: Snapshot transfer", "running", "default", 'Step 4 of 5 of the plan "Replicated log": Snapshot transfer', "Streaming a snapshot to a lagging follower.", {
    parent: "fan-lead",
    doing: "Streaming a 10 GB snapshot",
  }),
  member("fan-sign", "claude", "local", "Rotate the release signing key", "running", "default", "Rotate the release signing key and re-sign 0.2.1's binaries", "Re-signing the binaries.", { parent: "fan-lead", doing: "Signing kiln-x86_64-linux" }),
]) {
  family.set(m.info.id, m);
}
{
  const m = family.get("fan-s3");
  if (m) m.info.approvals = [{ id: fanApproval.id, tool: fanApproval.tool, tool_name: fanApproval.tool_name, title: fanApproval.title, at: fanApproval.at }];
}

// Release prep proposed a plan (plan mode), kept by kiln (pages.ts `pl_proposed`), and works through its to-do list.
{
  const m = family.get("fam-orch");
  m?.events.push({ id: "fam-orch-plan", seq: 3, at: t0 + 50_000, kind: "tool", tool_id: "tp1", name: "ExitPlanMode", tool: "plan", title: "Plan ready for review", input: { plan: PROPOSED }, status: "done", started: t0 + 50_000 } as unknown as HEvent);
  // It records what it found in its research (pages.ts `rs_quic`), as kiln's MCP tool.
  m?.events.push({
    id: "fam-orch-research",
    seq: 4,
    at: t0 + 55_000,
    kind: "tool",
    tool_id: "tr1",
    name: "mcp__kiln__research_add",
    tool: "mcp",
    server: "kiln",
    tool_short: "research_add",
    title: "research_add",
    input: { id: "rs_quic", findings: [{ claim: "hyper-raft's log transport takes any stream with ordered delivery, so a QUIC stream carries it as it is." }] },
    output: '{"id": "rs_quic", "question": "Add full hyperaraft dual UDP/QUIC stack replication"}',
    status: "done",
    started: t0 + 55_000,
    ended: t0 + 56_000,
  } as unknown as HEvent);
}
// Asked to plan three of its goals now (goals.ts, Make a plan), each by a subagent of its own: kiln's
// steer for each (`kiln-plan-…`, the plan's id in it), a subagent at "The planner" (its calls so far,
// the last still going), none yet for "Store and transfer", and one that stopped before "Optimize"
// was ready.
{
  const m = family.get("fam-orch");
  const now = Date.now();
  const brief = (plan: string, research: string, goal: string) =>
    `Plan the goal "${goal}" as plan ${plan}, its research ${research}. Research it inside and out, record what you find (research_add), write its steps (plans_update) and say it's ready (plans_ready). Report back the plan's title and steps.`;
  const ask = (plan: string, research: string, goal: string, seq: number, at: number) =>
    ({
      id: `fam-orch-ask-${plan}`,
      seq,
      at,
      kind: "user",
      client_id: `kiln-plan-${plan}`,
      origin: "kiln",
      text: `Your person asked for one of your goals to be planned now, beside what you're doing: "${goal}". Dispatch a subagent for it, give it the brief below, and go on with your own work meanwhile.\n\nThe brief: ${brief(plan, research, goal)}`,
    }) as HEvent;
  let seq = 5;
  const call = (parent: string | undefined, at: number, name: string, tool: string, title: string, input: unknown, going = false, extra: Record<string, unknown> = {}) =>
    ({
      id: `fam-orch-c${seq}`,
      seq: seq++,
      at,
      ...(parent && { parent }),
      kind: "tool",
      tool_id: `toolu_c${seq}`,
      name,
      tool,
      title,
      input,
      status: going ? "running" : "done",
      started: at,
      ...(!going && { ended: at + 1500 }),
      ...extra,
    }) as unknown as HEvent;
  const kiln = (tool: string) => ({ server: "kiln", tool_short: tool });
  const PLANNER = "The planner, researched and audited";
  const OPT = "Optimize the QUIC API and s3 HTTP API";
  m?.events.push(
    ask("pl_planner", "rs_planner", PLANNER, seq++, now - 80_000),
    call(
      undefined,
      now - 74_000,
      "Agent",
      "agent",
      "Plan the planner goal",
      { description: "Plan the planner goal", subagent_type: "general-purpose", prompt: brief("pl_planner", "rs_planner", PLANNER) },
      true,
      { task: "a-planner", tool_id: "toolu_planner" },
    ),
    call("toolu_planner", now - 70_000, "mcp__kiln__research_get", "mcp", "kiln · research_get", { id: "rs_planner" }, false, kiln("research_get")),
    call("toolu_planner", now - 62_000, "WebSearch", "web", "DPhyp join enumeration DuckDB optimizer", { query: "DPhyp join enumeration DuckDB optimizer" }),
    call("toolu_planner", now - 51_000, "WebFetch", "web", "https://duckdb.org/2024/11/14/optimizers.html", { url: "https://duckdb.org/2024/11/14/optimizers.html" }),
    call(
      "toolu_planner",
      now - 38_000,
      "mcp__kiln__research_add",
      "mcp",
      "kiln · research_add",
      {
        id: "rs_planner",
        findings: [
          { claim: "DuckDB enumerates join orders with DPhyp and falls back to a greedy order past 12 relations." },
          { claim: "Umbra's adaptive compilation picks its plan before it compiles." },
        ],
      },
      false,
      kiln("research_add"),
    ),
    call("toolu_planner", now - 21_000, "Grep", "search", "fn join_order", { pattern: "fn join_order" }),
    call("toolu_planner", now - 6_000, "Read", "read", "crates/amphora/src/planner/cost.rs", { file_path: "/home/ada/src/amphora/crates/amphora/src/planner/cost.rs" }, true),
    ask("pl_store", "rs_store", "Build the data storage/transfer layer", seq++, now - 25_000),
    ask("pl_opt", "rs_opt", OPT, seq++, now - 9 * 60_000),
    call(
      undefined,
      now - 8.8 * 60_000,
      "Agent",
      "agent",
      "Plan the API optimization goal",
      { description: "Plan the API optimization goal", subagent_type: "general-purpose", prompt: brief("pl_opt", "rs_opt", OPT) },
      false,
      { task: "a-opt", tool_id: "toolu_opt" },
    ),
    call(
      "toolu_opt",
      now - 8.5 * 60_000,
      "mcp__kiln__research_add",
      "mcp",
      "kiln · research_add",
      { id: "rs_opt", findings: [{ claim: "The s3 API's PUT path copies each part twice before it reaches the store." }, { claim: "hyper-quic's send path allocates a frame per write." }] },
      false,
      kiln("research_add"),
    ),
    call("toolu_opt", now - 6 * 60_000, "Bash", "shell", "ssh fedora 'cd ~/src/amphora && ./bench/run.sh s3-put'", { command: "ssh fedora 'cd ~/src/amphora && ./bench/run.sh s3-put'" }, false, {
      status: "error",
      exit_code: 255,
    }),
  );
  if (m) {
    m.tasks = [
      {
        id: "a-planner",
        tool_id: "toolu_planner",
        description: "Plan the planner goal",
        class: "agent",
        kind: "general-purpose",
        status: "running",
        started: now - 74_000,
        prompt: brief("pl_planner", "rs_planner", PLANNER),
        last_tool: "Read",
        tool_uses: 14,
        tokens: 48_200,
        stoppable: true,
      },
      {
        id: "a-opt",
        tool_id: "toolu_opt",
        description: "Plan the API optimization goal",
        class: "agent",
        kind: "general-purpose",
        status: "completed",
        started: now - 8.8 * 60_000,
        ended: now - 5.5 * 60_000,
        prompt: brief("pl_opt", "rs_opt", OPT),
        summary: "I couldn't reach fedora to measure the current read path (ssh: connection refused), so the plan isn't ready: 2 findings, no steps.",
        tool_uses: 9,
        tokens: 21_400,
        stoppable: true,
      },
    ];
    m.info.subagents = m.tasks.map((t): SubagentBrief => ({ id: t.id, tool_id: t.tool_id, description: t.description, status: t.status, started: t.started, ended: t.ended }));
    // "The planner"'s subagent at work: every few seconds its call under way ends and the next starts.
    const next: [string, string, string, unknown, Record<string, unknown>?][] = [
      ["WebSearch", "web", "Umbra adaptive query compilation cost model", { query: "Umbra adaptive query compilation cost model" }],
      [
        "mcp__kiln__research_add",
        "mcp",
        "kiln · research_add",
        { id: "rs_planner", findings: [{ claim: "DataFusion's join selection reorders by estimated rows, with no histograms." }] },
        kiln("research_add"),
      ],
      ["Read", "read", "crates/amphora/src/planner/joins.rs", { file_path: "/home/ada/src/amphora/crates/amphora/src/planner/joins.rs" }],
      [
        "mcp__kiln__plans_update",
        "mcp",
        "kiln · plans_update",
        { id: "pl_planner", items: [{ title: "Measure our planner on the TPC-H joins" }, { title: "Order joins by DPhyp, greedy past 12" }, { title: "Cost joins from distinct counts" }] },
        kiln("plans_update"),
      ],
      ["Grep", "search", "fn estimate_rows", { pattern: "fn estimate_rows" }],
      ["WebFetch", "web", "https://db.in.tum.de/~neumann/papers/umbra-cidr.pdf", { url: "https://db.in.tum.de/~neumann/papers/umbra-cidr.pdf" }],
    ];
    let k = 0;
    const task = m.tasks[0]!;
    const tick = setInterval(() => {
      if (task.status !== "running") return clearInterval(tick);
      const at = Date.now();
      const going = m.events.findLast((e) => e.parent === "toolu_planner" && e.kind === "tool" && e.status === "running");
      const done = going && ({ ...going, status: "done", ended: at } as HEvent);
      if (done) m.events[m.events.indexOf(going)] = done;
      const [name, tool, title, input, extra] = next[k++ % next.length]!;
      const fresh = call("toolu_planner", at, name, tool, title, input, true, extra);
      m.events.push(fresh);
      task.tool_uses = (task.tool_uses ?? 0) + 1;
      task.last_tool = name;
      const out = [JSON.stringify({ t: "events", events: [...(done ? [done] : []), fresh] }), JSON.stringify({ t: "state", state: stateOf(m) })];
      for (const ws of viewers.get(m.info.id) ?? []) if (ws.readyState === ws.OPEN) out.forEach((o) => ws.send(o));
    }, 4000);
    tick.unref?.();
  }
}
const ORCH_TODOS: Todo[] = [
  { text: "Reproduce it with the CLI test", status: "completed" },
  { text: "Key WritableOpens by handle and inode", status: "completed" },
  { text: "Adopt unseen handles on a restored mount", status: "in_progress" },
  { text: "Red-first tests: the CLI and coherence tests", status: "pending" },
  { text: "Clippy, the doc gate and CI", status: "pending" },
];

/* ------------------------------------------------------------------ a plan run, one agent per step */

const RUN = "pr_agents1";
const RUN_PLAN = "pl_release";
const RUN_TITLE = "Cut the 0.2 release";
for (const m of [
  member("run-freeze", "claude", "agent:metal-ryzen-01", "Step 1: Freeze main", "idle", "default", "Step 1 of 3 of the plan \"Cut the 0.2 release\": Freeze main — Stop merging features; only fixes from here. — Done when: Branch protection requires a release label.", "Branch protection now requires a release label on main.", {
    plan_run: RUN,
    last_activity: Date.now() - 6 * 60_000,
  }),
  member("run-bench", "codex", "agent:metal-epyc-01", "Step 2: Run the full bench", "running", "untrusted", "Step 2 of 3 of the plan \"Cut the 0.2 release\": Run the full bench — Compare against 0.1 on the metal runners.", "Running the bench against 0.1.", {
    plan_run: RUN,
    doing: "Running kiln run bench --all",
  }),
])
  family.set(m.info.id, m);

/* ------------------------------------------------------------------ what agents leave running */

const at = Date.now();
/** Its entry as the list has it, from its tasks: its subagents, and how many tasks still run. */
function leaves(m: Member) {
  const tasks = m.tasks ?? [];
  const live = tasks.filter((t) => t.status === "running" || t.status === "pending" || t.status === "paused");
  m.info.subagents = tasks
    .filter((t) => t.class === "agent")
    .map((t): SubagentBrief => ({ id: t.id, tool_id: t.tool_id, description: t.description, status: t.status, started: t.started, ended: t.ended }));
  m.info.tasks_running = live.length;
  m.info.left_behind = live.length;
}
for (const [m, tasks] of [
  [
    member(
      "sub-busy",
      "claude",
      "agent:metal-ryzen-01",
      "System page",
      "idle",
      "acceptEdits",
      "Bring the System page's figures, the table and the build logs up to date, and keep the mock running while you do.",
      "Committed locally. The subagent is still on the figures; I'll review its screenshots when it reports.",
      {
        last_activity: at - 3 * 60_000,
      },
    ),
    [
      {
        id: "a6f2e91c0b7d4e5f8",
        tool_id: "toolu_figs",
        description: "System page figures, table, build logs",
        class: "agent",
        kind: "local_agent",
        status: "running",
        background: true,
        started: at - 17 * 60_000,
        last_tool: "Bash",
        stoppable: true,
      },
      {
        id: "b7x4k2",
        tool_id: "toolu_mock",
        description: "Serve the mock",
        command: "npm exec tsx mock/server.ts",
        class: "shell",
        kind: "local_bash",
        status: "running",
        background: true,
        started: at - 7.5 * 60_000,
        cwd: "/home/ada/src/kiln/ui",
        output: "/tmp/claude-1000/tasks/b7x4k2.output",
        stoppable: true,
      },
      {
        id: "m9q1",
        description: "Wake me if build 412 fails",
        command: "tail -f build.log | grep -m1 ERROR",
        class: "monitor",
        kind: "local_bash",
        status: "running",
        started: at - 5 * 60_000,
        events: 3,
        last_event: "build 412: 61/80 tests",
        wakes: "agent",
        stoppable: true,
      },
    ],
  ],
  [
    member(
      "sub-done",
      "claude",
      "agent:metal-ryzen-02",
      "Release notes",
      "idle",
      "default",
      "Draft the 0.2.1 release notes from what merged this week, one subagent per area.",
      "The notes are in docs/releases/0.2.1.md: three areas, each checked by its subagent.",
      {
        last_activity: at - 2 * 60_000,
      },
    ),
    [
      {
        id: "c1",
        tool_id: "toolu_c1",
        description: "Notes for the scheduler",
        class: "agent",
        kind: "local_agent",
        status: "completed",
        started: at - 9 * 60_000,
        ended: at - 4 * 60_000,
        stoppable: true,
      },
      { id: "c2", tool_id: "toolu_c2", description: "Notes for the UI", class: "agent", kind: "local_agent", status: "completed", started: at - 9 * 60_000, ended: at - 3 * 60_000, stoppable: true },
    ],
  ],
  [
    member(
      "sub-codex",
      "codex",
      "agent:metal-epyc-01",
      "Scheduler audit",
      "idle",
      "untrusted",
      "Audit the scheduler's retry paths; send a subagent through the backoff code.",
      "The subagent is reading the backoff code; I'll fold in what it finds.",
      {
        last_activity: at - 60_000,
      },
    ),
    [
      {
        id: "019a-child",
        tool_id: "call_child",
        description: "Read the backoff code",
        class: "agent",
        kind: "subagent",
        status: "running",
        started: at - 4 * 60_000,
        last_tool: "rg backoff",
        stoppable: false,
      },
    ],
  ],
] as [Member, Task[]][]) {
  m.tasks = tasks;
  leaves(m);
  family.set(m.info.id, m);
}

/** A shell's output so far: a line a second while it runs, as a server's log grows. */
function shellOutput(t: Task): string {
  const lines = ["> kiln-ui@0.2.0 mock", "> tsx mock/server.ts", "", "mock kiln on http://localhost:7070 (MOCK_NO_AUTH)"];
  const n = Math.floor(((t.ended ?? Date.now()) - t.started) / 1000);
  const paths = ["/api/harness/sessions", "/api/overview", "/api/harness/sessions/sub-busy/goals", "/api/me", "/api/runners"];
  for (let i = 0; i < n; i++) lines.push(`${new Date(t.started + i * 1000).toISOString().slice(11, 19)} GET ${paths[i % paths.length]} 200 ${2 + ((i * 7) % 11)}ms`);
  if (t.ended) lines.push("^C", "stopped");
  return `${lines.join("\n")}\n`;
}

/** A task's output (`taskOutput`), as the server reads it: a shell's file; a monitor keeps none. */
function taskOutput(m: Member, id: string, since: number | undefined) {
  const t = m.tasks?.find((x) => x.id === id);
  if (!t) return { error: "no such task in this session" };
  const live = t.status === "running" || t.status === "pending" || t.status === "paused";
  if (t.class !== "shell") return { data: "", offset: since ?? 0, live, gone: true };
  const text = shellOutput(t);
  const from = since ?? Math.max(0, text.length - 64 * 1024);
  return { data: text.slice(from), offset: text.length, live, truncated: from > 0 && since === undefined };
}

/** Each member's open pages: what changes reaches them. */
const viewers = new Map<string, Set<WebSocket>>();

/** Stop one of its tasks, as its harness would: it ends, and its entry says so. */
function stopTask(m: Member, id: string) {
  const t = m.tasks?.find((x) => x.id === id);
  if (!t || !(t.status === "running" || t.status === "pending" || t.status === "paused") || !t.stoppable) return;
  t.status = "stopped";
  t.ended = Date.now();
  leaves(m);
  // A subagent stopped is reported back to it: the last word is new.
  if (t.class === "agent") m.info.last_activity = Date.now();
  const state = JSON.stringify({ t: "state", state: stateOf(m) });
  for (const ws of viewers.get(m.info.id) ?? []) if (ws.readyState === ws.OPEN) ws.send(state);
  emitEvent({ type: "harness", info: m.info });
}

interface MockStep {
  title: string;
  goal: string;
  /** The steps it waits for, by number. */
  needs: number[];
  status: "waiting" | "started" | "taken" | "met" | "unmet" | "stopped";
  agent: string | null;
  reason: string | null;
  tries: number;
  started_at: number | null;
  ended_at: number | null;
}
const run = {
  state: "running" as "running" | "stopped" | "done",
  reason: null as string | null,
  at: Math.floor((t0 + 30_000) / 1000),
  steps: [
    { title: "Freeze main", goal: family.get("run-freeze")?.info.goal ?? "", needs: [], status: "met", agent: "run-freeze", reason: "the agent said so: branch protection requires a release label", tries: 1, started_at: null, ended_at: null },
    { title: "Run the full bench", goal: family.get("run-bench")?.info.goal ?? "", needs: [1], status: "started", agent: "run-bench", reason: null, tries: 1, started_at: null, ended_at: null },
    { title: "Tag and publish", goal: 'Step 3 of 3 of the plan "Cut the 0.2 release": Tag and publish', needs: [1, 2], status: "waiting", agent: null, reason: null, tries: 0, started_at: null, ended_at: null },
  ] as MockStep[],
};

function runView() {
  const steps = run.steps.map((s, i) => {
    const m = s.agent ? family.get(s.agent) : undefined;
    const stand = s.status === "started" ? (m && (m.info.approvals.length || m.info.status === "waiting") ? "needs_you" : "working") : s.status === "taken" ? "yours" : s.status;
    return { n: i + 1, title: s.title, goal: s.goal, stand, needs: s.needs, given: false, agent: m ? brief(m.info) : null, agent_id: s.agent, started_at: s.started_at, ended_at: s.ended_at, reason: s.reason, tries: s.tries };
  });
  const met = steps.filter((s) => s.stand === "met").length;
  // Each met step's diff, on the one before's (docs/agentic.md P4).
  const stacked = run.steps
    .map((s, i) => ({ s, n: i + 1 }))
    .filter(({ s }) => s.status === "met")
    .map(({ n }) => ({ n, state: "made", change: `D${41 + n}`, url: `/diffs/D${41 + n}`, sha: `4b5c6d7e8f${n}a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d`, branch: null, files: n === 1 ? [".github/branch-protection.yml"] : ["bench/results/0.2.json"], pushed: true, error: null }));
  const stack = stacked.length ? { off: null, provider: "kiln", repo: "hyper-light/kiln", trunk: "main", base: "1c2d3e4f5a6b7c8d9e0f1a2b3c4d5e6f7a8b9c0d", steps: stacked } : null;
  return { id: RUN, plan: RUN_PLAN, title: RUN_TITLE, version: 4, state: run.state, reason: run.reason, note: null, at: run.at, ended_at: null, by: "admin", config: {}, steps, met, total: steps.length, stack };
}

/** The run's routes (before the plans' own, whose `:id` would take "runs"). */
async function handleRun(req: http.IncomingMessage, res: http.ServerResponse, url: URL): Promise<boolean> {
  const p = url.pathname;
  const m = req.method ?? "GET";
  if (p === "/api/plans/runs" && m === "GET") {
    const v = runView();
    const runs = [{ id: RUN, plan: RUN_PLAN, title: RUN_TITLE, state: v.state, met: v.met, total: v.total, at: v.at }];
    json(res, 200, { runs: url.searchParams.get("active") ? runs.filter((r) => r.state === "running") : runs });
    return true;
  }
  const rm = p.match(/^\/api\/plans\/runs\/([^/]+)(?:\/stop|\/steps\/(\d+)\/(retry|take|met))?$/);
  if (!rm) return false;
  if (decodeURIComponent(rm[1] ?? "") !== RUN) {
    json(res, 404, { error: `no plan run ${rm[1]}` });
    return true;
  }
  if (m === "GET") {
    json(res, 200, runView());
    return true;
  }
  if (p.endsWith("/stop")) {
    run.state = "stopped";
    run.reason = "stopped by admin";
    for (const s of run.steps.filter((x) => x.status === "started")) {
      s.status = "stopped";
      s.reason = "stopped by admin";
    }
  } else {
    const s = run.steps[Number(rm[2]) - 1];
    if (!s) return (json(res, 400, { error: "no such step" }), true);
    if (rm[3] === "take") {
      s.status = "taken";
      s.reason = "taken over by admin";
    } else if (rm[3] === "met") {
      s.status = "met";
      s.reason = "met, said admin";
      const next = run.steps.find((x) => x.status === "waiting");
      if (next) next.status = "started";
    } else {
      s.status = "started";
      s.tries += 1;
      s.reason = null;
    }
    run.state = "running";
    run.reason = null;
  }
  emitEvent({ type: "harness", info: family.get("run-bench")?.info });
  json(res, 200, runView());
  return true;
}

/** The family, for the agents list. */
export function familyInfos(): AgentInfo[] {
  return [...family.values()].map((m) => m.info);
}

function stand(i: AgentInfo): string {
  if (i.approvals.length > 0 || i.status === "waiting") return "needs_you";
  if (i.status === "running" || i.status === "starting") return "working";
  if (i.status === "exited") return i.exit?.error ? "stopped" : "ended";
  return i.status === "disconnected" ? "offline" : "idle";
}

function brief(i: AgentInfo) {
  return {
    id: i.id,
    name: i.name,
    kind: i.kind,
    machine: i.target.replace(/^agent:/, ""),
    cwd: i.cwd,
    status: i.status,
    stand: stand(i),
    mode: i.mode,
    model: i.model,
    queued: i.queued,
    summary: i.summary ?? "",
    files_changed: i.files_changed ?? 0,
    lines_added: i.lines_added ?? 0,
    lines_removed: i.lines_removed ?? 0,
    cost_usd: i.cost_usd,
    ...(i.parent && { parent: i.parent }),
    ...(i.plan_run && { plan_run: i.plan_run }),
    ...(i.approvals.length > 0 && { needs: i.approvals.map((a) => ({ id: a.id, tool: a.tool, tool_name: a.tool_name, title: a.title })) }),
  };
}

/** The agents an agent started that still run, and theirs. */
function descendants(id: string, all: AgentInfo[], live: boolean): AgentInfo[] {
  const out: AgentInfo[] = [];
  const todo = [id];
  while (todo.length) {
    const p = todo.pop();
    for (const c of all.filter((a) => a.parent === p)) {
      if (out.some((o) => o.id === c.id)) continue;
      todo.push(c.id);
      if (!live || c.status !== "exited") out.push(c);
    }
  }
  return out;
}

const META: Meta = {
  version: "2.3.1",
  modes: [
    { id: "plan", label: "Plan", description: "Reads and plans; changes nothing" },
    { id: "default", label: "Ask", description: "Asks before edits and commands" },
    { id: "acceptEdits", label: "Accept edits", description: "Edits files without asking; asks before commands" },
  ],
  models: [],
  commands: [{ name: "btw", description: "Ask a side question without interrupting", hint: "<question>", group: "Session", args: "required", route: { kind: "prompt" } }],
  agents: [],
  features: ["interrupt", "set_mode", "steer"],
};

/** How a person's message reaches one of its subagents, as its harness lets one in (harness/subagent.rs): a Codex one on its own thread, a Claude Code one by way of its agent (not once stopped). */
function withTakes(m: Member, t: Task): Task {
  if (t.class !== "agent" || !t.tool_id) return t;
  const live = t.status === "running" || t.status === "pending" || t.status === "paused";
  if (m.info.kind === "codex") return { ...t, takes: live ? ["queue", "steer", "now"] : ["queue"] };
  if (t.status === "stopped" || t.status === "killed") return t;
  return { ...t, takes: live && t.background ? ["queue", "steer"] : ["queue"], via: "parent" };
}

/** A person's message to subagent `task`: in its thread, or waiting in its queue for it to finish. */
function tellSubagent(m: Member, task: string, id: string, text: string, mode: string) {
  const t = m.tasks?.find((x) => x.id === task);
  if (!t?.tool_id) return;
  const live = t.status === "running" || t.status === "pending" || t.status === "paused";
  const out: unknown[] = [];
  if (mode === "queue" && live) t.queue = [...(t.queue ?? []), { id, text, at: Date.now() }];
  else {
    const e = { id, seq: Math.max(0, ...m.events.map((x) => x.seq)) + 1, at: Date.now(), parent: t.tool_id, kind: "user", text, client_id: id } as HEvent;
    m.events.push(e);
    out.push({ t: "events", events: [e] });
  }
  out.unshift({ t: "state", state: stateOf(m) });
  for (const ws of viewers.get(m.info.id) ?? []) if (ws.readyState === ws.OPEN) for (const u of out) ws.send(JSON.stringify(u));
}

/** A command about a message waiting in one of its subagents' queues (harness/subagent.rs): edited, moved or taken out there, or sent to it now. */
function subagentQueue(m: Member, c: { type?: string; id?: string; text?: string; index?: number }) {
  const t = m.tasks?.find((x) => x.queue?.some((q) => q.id === c.id));
  const queue = t?.queue ?? [];
  const at = queue.findIndex((q) => q.id === c.id);
  const q = queue[at];
  if (!t || !q) return;
  if (c.type === "queue_edit" && c.text !== undefined) q.text = c.text;
  else if (c.type === "queue_move" && c.index !== undefined) queue.splice(Math.max(0, Math.min(c.index, queue.length - 1)), 0, ...queue.splice(at, 1));
  else if (c.type === "queue_remove" || c.type === "queue_steer" || c.type === "queue_send_now") queue.splice(at, 1);
  else return;
  if (c.type === "queue_steer" || c.type === "queue_send_now") return tellSubagent(m, t.id, q.id, q.text, "steer");
  const state = JSON.stringify({ t: "state", state: stateOf(m) });
  for (const ws of viewers.get(m.info.id) ?? []) if (ws.readyState === ws.OPEN) ws.send(state);
}

function stateOf(m: Member): State {
  return {
    status: m.info.status,
    cwd: m.info.cwd,
    model: m.info.model,
    mode: m.info.mode,
    title: m.info.title,
    summary: m.info.summary,
    doing: m.info.doing,
    approvals: m.approval ? [m.approval] : [],
    queue: [],
    delivery: {},
    tasks: (m.tasks ?? []).map((t) => withTakes(m, t)),
    todos: m.info.id === "fam-orch" ? ORCH_TODOS : [],
    usage: { cost_usd: m.info.cost_usd, input_tokens: 0, output_tokens: 0, cache_read_tokens: 0, turns: 1 },
    dropped: 0,
  } as State;
}

/** A family member's socket: its snapshot. */
export function handleFamilySession(ws: WebSocket, id: string): boolean {
  const m = family.get(id);
  if (!m) return false;
  const send = (u: unknown) => ws.send(JSON.stringify(u));
  const seen = viewers.get(id) ?? new Set<WebSocket>();
  viewers.set(id, seen.add(ws));
  ws.on("close", () => seen.delete(ws));
  ws.on("message", (raw) => {
    try {
      const c = JSON.parse(raw.toString()) as { type?: string; task?: string; id?: string; text?: string; mode?: string; index?: number };
      if (c.type === "stop_task" && c.task) setTimeout(() => stopTask(m, c.task ?? ""), 700);
      if (c.type === "send" && c.task && c.text) tellSubagent(m, c.task, c.id ?? `sub-${Date.now()}`, c.text, c.mode ?? "queue");
      if (c.type?.startsWith("queue_")) subagentQueue(m, c);
    } catch {
      // Not a command.
    }
  });
  send({ t: "open", info: m.info });
  send({ t: "meta", meta: { ...META, version: m.info.kind === "claude" ? "2.3.1" : "0.46.0" } });
  send({ t: "state", state: stateOf(m) });
  send({ t: "events", events: m.events });
  const files = CHANGESETS[id];
  if (files) send({ t: "changes", files });
  if (process.env.MOCK_NO_SYNCED !== "1") send({ t: "synced" });
  return true;
}

/** What the family's agents changed, as their machines report it. */
const CHANGESETS: Record<string, unknown[]> = {
  "fam-tests": [
    {
      path: "/home/ada/.local/state/kiln/workspaces/kiln-famtests/crates/kiln/src/http/client.rs",
      added: 6,
      removed: 2,
      at: Date.now() - 600_000,
      hunks: [
        {
          old_start: 212,
          old_lines: 6,
          new_start: 212,
          new_lines: 10,
          lines: [
            "     let mut tries = 0;",
            "-    loop {",
            "-        match send(&req).await {",
            "+    // Each try waits on its own deadline, so a slow first answer can't use up the next's.",
            "+    loop {",
            "+        let deadline = Instant::now() + per_try;",
            "+        match timeout_at(deadline, send(&req)).await {",
            "             Ok(r) => return Ok(r),",
            "+            Err(_) if tries + 1 < max => {}",
            "             Err(e) if tries + 1 >= max => return Err(e),",
          ],
        },
      ],
    },
  ],
};

function end(id: string): boolean {
  if (!family.delete(id)) return false;
  emitEvent({ type: "harness_closed", id });
  return true;
}

/** Agents' change requests (`agents.change`), by agent: the flaky-test fixer's is up for review. */
const changes = new Map<string, Record<string, unknown>>([
  [
    "fam-tests",
    {
      agent: "fam-tests",
      repo: "hyper-light/kiln",
      provider: "kiln",
      change: "CR7",
      url: "/diffs/CR7",
      branch: "agents/fix-the-flaky-retry-test-famtests",
      trunk: "main",
      sha: "9f3c2e1a7b6d5c4e3f2a1b0c9d8e7f6a5b4c3d2e",
      version: 2,
      files: ["crates/kiln/src/http/client.rs"],
      title: "Fix the flaky retry test",
      state: "open",
      review: "needs-review",
      checks: "failed",
      fix_checks: true,
      opened_at: Math.floor(Date.now() / 1000) - 3600,
      updated_at: Math.floor(Date.now() / 1000) - 600,
    },
  ],
  [
    "run-freeze",
    {
      agent: "run-freeze",
      repo: "hyper-light/kiln",
      provider: "kiln",
      change: "D42",
      url: "/diffs/D42",
      branch: "",
      trunk: "main",
      sha: "4b5c6d7e8f1a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d",
      version: 1,
      files: [".github/branch-protection.yml"],
      title: "Freeze main",
      state: "open",
      review: "approved",
      checks: "passed",
      fix_checks: true,
      opened_at: Math.floor(Date.now() / 1000) - 360,
      updated_at: Math.floor(Date.now() / 1000) - 120,
      stack: { run: "pr_agents1", step: 1 },
    },
  ],
]);

/** The `agents.*` routes. True when it answered. */
/* ------------------------------------------------------------------ attached agents (W20-R08) */

const now = () => Math.floor(Date.now() / 1000);
family.set(
  "att-ci",
  member("att-ci", "claude", "agent:metal-ryzen-01", "ci-doctor: CI failed on main: assertion failed in sync::reconnect", "running", "default", "You are ci-doctor, attached to repo:hyper-light/kiln in kiln, and this woke you (failure): CI failed on main.", "Reproducing the reconnect failure locally.", {
    doing: "Running cargo test -p kiln sync::reconnect",
  }),
);
const attached = new Map<string, Record<string, unknown>>([
  [
    "a1",
    { id: "a1", name: "ci-doctor", on: "repo:hyper-light/kiln", branches: ["main"], harness: { kind: "claude", on: "metal-ryzen-01", cwd: "/home/ada/src/kiln" }, wake: ["failure", "flaky", "mention"], can: ["read", "rerun", "fix"], limits: { sessions_per_day: 10, reruns_per_failure: 2, stuck_minutes: 15 }, playbook: "Rerun what's flaky once; for a regression, open a revert as a diff.", reviewers: ["ada"], by: "owner", paused: false, created_at: now() - 86_400, updated_at: now() - 86_400 },
  ],
  [
    "a2",
    { id: "a2", name: "runner-doctor", on: "runners", branches: [], harness: { kind: "codex" }, wake: ["offline", "upgrade"], can: ["read", "diagnose", "manage-runners"], limits: { sessions_per_day: 10, reruns_per_failure: 2, stuck_minutes: 15 }, playbook: "", reviewers: [], by: "owner", paused: false, created_at: now() - 3 * 86_400, updated_at: now() - 3 * 86_400 },
  ],
  [
    "hyper-light__kiln.release-watch",
    {
      id: "hyper-light__kiln.release-watch",
      name: "release-watch",
      on: "repo:hyper-light/kiln",
      branches: [],
      workflows: ["release"],
      source: "hyper-light/kiln:.kiln/agents.yaml",
      harness: { kind: "claude" },
      wake: ["failure"],
      can: ["read", "rerun"],
      limits: { sessions_per_day: 5, reruns_per_failure: 1, stuck_minutes: 15 },
      playbook: "",
      reviewers: [],
      by: "owner",
      paused: false,
      created_at: 0,
      updated_at: 0,
    },
  ],
  [
    "a3",
    { id: "a3", name: "nightly-gardener", on: "workflow:hyper-light/kiln:.github/workflows/nightly.yml", branches: [], harness: { kind: "codex" }, wake: ["schedule"], schedule: "0 6 * * 1", can: ["read", "edit-pipelines"], limits: { sessions_per_day: 1, reruns_per_failure: 0, stuck_minutes: 15 }, playbook: "Shorten the critical path.", reviewers: ["ada"], by: "owner", paused: true, created_at: now() - 7 * 86_400, updated_at: now() - 2 * 86_400 },
  ],
]);
const incidentList: Record<string, unknown>[] = [
  { id: "i3", attachment: "a1", name: "ci-doctor", on: "repo:hyper-light/kiln", wake: "failure", repo: "hyper-light/kiln", title: "CI failed on main: assertion failed in sync::reconnect", state: "open", agent: "att-ci", class: null, report: null, links: [], why: null, times: 2, last: { at: now() - 240, what: "CI #412" }, at: now() - 900, updated_at: now() - 240 },
  { id: "i2", attachment: "a2", name: "runner-doctor", on: "runners", wake: "upgrade", runner: "metal-epyc-01", title: "Upgrading metal-epyc-01 failed: cargo build exited 101", state: "waiting", agent: null, class: null, report: null, links: [], why: "its agent couldn't start: no machine suits codex: none is connected", times: 1, last: { at: now() - 7200, what: "upgrade u9 (build)" }, at: now() - 7200, updated_at: now() - 7200 },
  { id: "i1", attachment: "a1", name: "ci-doctor", on: "repo:hyper-light/kiln", wake: "flaky", title: "Flaky in CI #409: retry::backoff_caps", state: "resolved", agent: "fam-tests", class: "flaky", report: "retry::backoff_caps races the clock; quarantined it (D41) and opened a fix as D44.", links: ["D41", "D44"], why: null, times: 1, last: { at: now() - 86_000, what: "CI #409" }, at: now() - 86_000, updated_at: now() - 80_000 },
];

/** The attachments' and incidents' routes. True when it answered. */
async function handleAttached(req: http.IncomingMessage, res: http.ServerResponse, url: URL): Promise<boolean> {
  const p = url.pathname;
  const m = req.method ?? "GET";
  const rows = () =>
    [...attached.values()]
      .sort((a, b) => String(a.name).localeCompare(String(b.name)))
      .map((a) => {
        const mine = incidentList.filter((i) => i.attachment === a.id);
        return { attachment: a, open: mine.filter((i) => i.state === "open").length, today: mine.filter((i) => i.agent && Number(i.at) > now() - 86_400).length, latest: mine[0] ?? null };
      });
  if (p === "/api/attachments" && m === "GET") return (json(res, 200, { attachments: rows() }), true);
  if (p === "/api/incidents" && m === "GET") {
    // A run's: those it happened in.
    const run = url.searchParams.get("run");
    return (json(res, 200, { incidents: run ? incidentList.filter((i) => (i.last as { run?: string } | undefined)?.run === run) : incidentList }), true);
  }
  if (p === "/api/attachments/pause" && m === "POST") {
    const b = await body<{ attachment?: string; paused?: boolean }>(req);
    const a = b.attachment ? attached.get(b.attachment) : undefined;
    if (!a) return (json(res, 404, { error: "no such attachment" }), true);
    a.paused = b.paused ?? true;
    emitEvent({ type: "attachment", attachment: a });
    return (json(res, 200, { paused: a.paused, attachments: [a.name] }), true);
  }
  if (p === "/api/attachments" && m === "POST") {
    const b = await body<Record<string, unknown>>(req);
    if (!String(b.name ?? "").trim()) return (json(res, 400, { error: "`name`: up to 40 letters, digits and `-` (people mention it as @name)" }), true);
    if ([...attached.values()].some((a) => a.name === b.name)) return (json(res, 409, { error: `an attachment is called ${String(b.name)} already` }), true);
    const id = `a${attached.size + 1}`;
    const a = { ...b, id, by: "owner", paused: false, created_at: now(), updated_at: now() };
    attached.set(id, a);
    emitEvent({ type: "attachment", attachment: a });
    return (json(res, 200, { attachment: a, grants: [] }), true);
  }
  const am = p.match(/^\/api\/attachments\/([^/]+)$/);
  if (am && m === "DELETE") {
    attached.delete(decodeURIComponent(am[1] ?? ""));
    emitEvent({ type: "attachment", removed: am[1] });
    return (json(res, 200, { removed: am[1] }), true);
  }
  return false;
}

export async function handleFamily(req: http.IncomingMessage, res: http.ServerResponse, url: URL, others: () => AgentInfo[]): Promise<boolean> {
  if (await handleRun(req, res, url)) return true;
  if (await handleAttached(req, res, url)) return true;
  const p = url.pathname;
  const m = req.method ?? "GET";
  const all = () => [...familyInfos(), ...others()];
  const om = p.match(/^\/api\/harness\/sessions\/([^/]+)\/tasks\/([^/]+)\/output$/);
  const owner = om ? family.get(decodeURIComponent(om[1] ?? "")) : undefined;
  if (om && owner && m === "GET") {
    const since = url.searchParams.get("since");
    json(res, 200, taskOutput(owner, decodeURIComponent(om[2] ?? ""), since === null ? undefined : Number(since)));
    return true;
  }
  if (p === "/api/harness/workspaces" && m === "GET") {
    json(res, 200, {
      local: { available: true },
      "agent:metal-ryzen-01": { available: true },
      "agent:metal-ryzen-02": { available: false, why_not: "Slates isn't installed on this machine" },
      "agent:metal-epyc-01": { available: false, why_not: "fusermount3 isn't installed (the fuse3 package)" },
    });
    return true;
  }
  if (p === "/api/harness/changes" && m === "GET") {
    const one = url.searchParams.get("agent");
    json(res, 200, { changes: [...changes.values()].filter((c) => !one || c.agent === one) });
    return true;
  }
  if (p === "/api/harness/agents" && m === "GET") {
    const parent = url.searchParams.get("parent");
    const run = url.searchParams.get("plan_run");
    json(res, 200, {
      agents: all()
        .filter((a) => (!parent || a.parent === parent) && (!run || a.plan_run === run))
        .map(brief),
    });
    return true;
  }
  if (p === "/api/harness/agents" && m === "POST") {
    json(res, 409, { error: "the mock starts no agents: use the demo ones" });
    return true;
  }
  if (p === "/api/harness/agents/wait" && m === "GET") {
    const ids = (url.searchParams.get("agents") ?? url.searchParams.get("agent") ?? "").split(",").filter(Boolean);
    json(res, 200, {
      done: true,
      timed_out: false,
      waited_ms: 0,
      until: "settled",
      agents: ids.map((id) => all().find((a) => a.id === id)).map((a, i) => (a ? brief(a) : { id: ids[i], stand: "ended", closed: true })),
    });
    return true;
  }
  const am = p.match(/^\/api\/harness\/agents\/([^/]+)(?:\/(result|children|stop|send|interrupt|change|workspace|land))?$/);
  if (!am) return false;
  const id = decodeURIComponent(am[1] ?? "");
  const what = am[2];
  const a = all().find((x) => x.id === id);
  if (!a) {
    json(res, 404, { error: `no agent ${id}` });
    return true;
  }
  if (!what && m === "GET") json(res, 200, { agent: brief(a), info: a, state: family.has(id) ? stateOf(family.get(id)!) : {} });
  else if (what === "children" && m === "GET") json(res, 200, { children: descendants(id, all(), true).map((c) => ({ id: c.id, name: c.name, kind: c.kind, status: c.status, parent: c.parent })) });
  else if (what === "result" && m === "GET") {
    json(res, 200, {
      agent: brief(a),
      reply: { text: a.summary ?? "", at: a.last_activity },
      changes: { files: [], files_changed: a.files_changed ?? 0, lines_added: a.lines_added ?? 0, lines_removed: a.lines_removed ?? 0 },
      cwd: a.cwd,
      machine: a.target.replace(/^agent:/, ""),
      cost_usd: a.cost_usd,
      workspace: null,
      branch: changes.get(id)?.branch ?? null,
      change_request: changes.get(id) ?? null,
    });
  } else if (what === "workspace" && m === "GET") {
    json(res, 200, { workspace: a.workspace ?? null, drift: a.workspace ? ["/crates/kiln/src/http/client.rs (Modified)"] : [] });
  } else if (what === "land" && m === "POST") {
    if (!a.workspace?.made) {
      json(res, 409, { error: `${a.name} works in its folder itself: what it changed is there already` });
      return true;
    }
    const b = await body<{ manifest?: string }>(req);
    if (b.manifest === undefined) {
      json(res, 200, {
        plan: {
          grant_required: true,
          landing: 7,
          manifest: "7b9cf3d31bed49590dce5886cb9c9fb21b2efe12ef21cdf3927ca5cd73ef9034",
          summary: { by_action: [{ action: "replace", count: 1 }, { action: "create", count: 1 }], bytes: 2214, filtered_out: 1834 },
          conflicts: [],
        },
        excludes: ["/.git"],
        can_write: false,
        why_not: "Slates on Linux can't take a person's go-ahead from kiln yet (Slates' GAP-A9-10): put the work up for review instead, or approve it with `slates grant` in the session that started Slates",
      });
    } else json(res, 200, { landed: { grant_required: false, outcome: { written: 2 } } });
  } else if (what === "change" && m === "POST") {
    if (!(a.files_changed ?? 0)) {
      json(res, 409, { error: `${a.name} hasn't changed any files yet` });
      return true;
    }
    const b = await body<{ title?: string; fix_checks?: boolean }>(req);
    const was = changes.get(id);
    const now = Math.floor(Date.now() / 1000);
    const slug = (b.title || a.title || a.name).toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "").slice(0, 40);
    const c = was
      ? { ...was, version: Number(was.version) + 1, checks: undefined, updated_at: now, sha: Math.random().toString(16).slice(2).padEnd(40, "0") }
      : {
          agent: id,
          repo: "hyper-light/kiln",
          provider: "kiln",
          change: "CR44",
          url: "/diffs/CR44",
          branch: `agents/${slug}-${id.replace(/[^a-z0-9]/gi, "").slice(0, 8)}`,
          trunk: "main",
          sha: Math.random().toString(16).slice(2).padEnd(40, "0"),
          version: 1,
          files: ["docs/agents.md"],
          title: b.title || a.title || a.name,
          state: "open",
          review: "needs-review",
          fix_checks: b.fix_checks ?? true,
          opened_at: now,
          updated_at: now,
        };
    changes.set(id, c);
    emitEvent({ type: "agent_change", agent: id, change: c });
    json(res, 200, { change_request: c });
  } else if (what === "stop" && m === "POST") {
    const b = await body<{ children?: boolean }>(req);
    const ended = [...(b.children ? descendants(id, all(), false).reverse() : []).filter((c) => end(c.id)).map((c) => c.id), ...(end(id) ? [id] : [])];
    json(res, 200, { ended });
  } else if ((what === "send" || what === "interrupt") && m === "POST") json(res, 200, what === "send" ? { sent: true, message: `m${Date.now()}` } : { interrupted: true });
  else json(res, 405, { error: "not here" });
  return true;
}
