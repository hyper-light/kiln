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
 * The `agents.*` routes answer as the server's do, from these and nothing else.
 */
import type http from "node:http";
import type { WebSocket } from "ws";
import type { AgentInfo, Approval, HEvent, Meta, State } from "../src/api/harness.ts";

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
  commands: [],
  agents: [],
  features: ["interrupt", "set_mode", "steer"],
};

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
    tasks: [],
    todos: [],
    usage: { cost_usd: m.info.cost_usd, input_tokens: 0, output_tokens: 0, cache_read_tokens: 0, turns: 1 },
    dropped: 0,
  } as State;
}

/** A family member's socket: its snapshot. */
export function handleFamilySession(ws: WebSocket, id: string): boolean {
  const m = family.get(id);
  if (!m) return false;
  const send = (u: unknown) => ws.send(JSON.stringify(u));
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
