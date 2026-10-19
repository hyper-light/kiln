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
      { parent: "fam-orch", doing: "Running cargo test -p kiln retry", files_changed: 1, lines_added: 6, lines_removed: 2 },
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
  if (process.env.MOCK_NO_SYNCED !== "1") send({ t: "synced" });
  return true;
}

function end(id: string): boolean {
  if (!family.delete(id)) return false;
  emitEvent({ type: "harness_closed", id });
  return true;
}

/** The `agents.*` routes. True when it answered. */
export async function handleFamily(req: http.IncomingMessage, res: http.ServerResponse, url: URL, others: () => AgentInfo[]): Promise<boolean> {
  const p = url.pathname;
  const m = req.method ?? "GET";
  const all = () => [...familyInfos(), ...others()];
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
  const am = p.match(/^\/api\/harness\/agents\/([^/]+)(?:\/(result|children|stop|send|interrupt))?$/);
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
      branch: null,
      change_request: null,
    });
  } else if (what === "stop" && m === "POST") {
    const b = await body<{ children?: boolean }>(req);
    const ended = [...(b.children ? descendants(id, all(), false).reverse() : []).filter((c) => end(c.id)).map((c) => c.id), ...(end(id) ? [id] : [])];
    json(res, 200, { ended });
  } else if ((what === "send" || what === "interrupt") && m === "POST") json(res, 200, what === "send" ? { sent: true, message: `m${Date.now()}` } : { interrupted: true });
  else json(res, 405, { error: "not here" });
  return true;
}
