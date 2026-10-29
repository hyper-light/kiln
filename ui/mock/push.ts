/**
 * The mock's notifications (docs/notifications.md): `/api/push` as kiln's server answers it, so
 * Settings → Notifications, Mission control's offer and an agent's Mute work without the Rust
 * server. It can't deliver a real push: what it would send is logged, and kept at
 * `GET /api/mock/push/outbox` (newest first).
 *
 * `MOCK_PUSH_FLEET=1` adds agents standing every way Mission control shows (needs you first, a
 * question, one stopped, one paused, some working…), for a list worth sorting on a phone.
 */
import type http from "node:http";
import { createECDH, randomBytes } from "node:crypto";
import type { AgentInfo } from "../src/api/harness.ts";
import { defaultInfo } from "./defaults.ts";

type Kind = "agent.needs_you" | "agent.finished" | "agent.stopped" | "agent.paused" | "machine.battery_low";

const KINDS: { id: Kind; label: string; means: string }[] = [
  { id: "agent.needs_you", label: "Needs you", means: "an approval, a question or a plan waits on you" },
  { id: "agent.finished", label: "Finished", means: "a turn ended" },
  { id: "agent.stopped", label: "Stopped", means: "it crashed or couldn't start" },
  { id: "agent.paused", label: "Paused", means: "it reached a usage limit" },
  { id: "machine.battery_low", label: "Low battery", means: "a machine your agents run on is low" },
];

/** A real P-256 key, so a browser can subscribe with it (nothing is ever signed with it here). */
const ecdh = createECDH("prime256v1");
ecdh.generateKeys();
const PUBLIC_KEY = ecdh.getPublicKey().toString("base64url");

interface Device {
  id: string;
  endpoint: string;
  device: string;
  hears: Kind[];
  created_at: number;
  last_ok_at: number | null;
  last_error: string | null;
  last_error_at: number | null;
}

const devices = new Map<string, Device>();
const muted = new Set<string>();
/** Your marks (`/api/push/notices`): what you put away, acted on or saw, by key. */
const marks = new Map<string, { key: string; how: "away" | "done" | "seen"; at: number }>();
interface Sent {
  at: number;
  to: string[];
  title: string;
  body: string;
  tag: string;
  url: string;
  kind: string;
}
const outbox: Sent[] = [];

interface Ctx {
  json: (res: http.ServerResponse, status: number, body: unknown) => void;
  err: (res: http.ServerResponse, status: number, message: string) => void;
  body: <T>(req: http.IncomingMessage) => Promise<T>;
  emit: (ev: { type: "push" } | { type: "harness"; info: AgentInfo } | { type: "notices"; principal: string; keys: string[]; how: "away" | "done" | "seen"; at: number; tags: string[] }) => void;
  noContent: (res: http.ServerResponse) => void;
}
let emitPush: () => void = () => {};

const now = () => Math.floor(Date.now() / 1000);

/** What it would send, to whom: logged and kept. */
function wouldSend(kind: Kind | "test", note: { title: string; body: string; tag: string; url: string }, only?: (d: Device) => boolean) {
  const to = [...devices.values()].filter((d) => (kind === "test" || d.hears.includes(kind)) && (!only || only(d)));
  if (to.length === 0) return 0;
  const sent = { at: Date.now(), to: to.map((d) => d.device), kind, ...note };
  outbox.unshift(sent);
  outbox.length = Math.min(outbox.length, 200);
  for (const d of to) d.last_ok_at = now();
  console.log(`[push] would send ${kind} to ${sent.to.join(", ")}: ${note.title} · ${note.body}`);
  emitPush();
  return to.length;
}

/* ---------------------------- the agents' changes, as the server watches them ---------------------------- */

type Stand = "needs" | "stopped" | "paused" | "working" | "idle" | "offline" | "ended";
const standOf = (a: AgentInfo): Stand =>
  a.approvals.length > 0 || a.status === "waiting"
    ? "needs"
    : a.status === "exited" && a.exit?.error
      ? "stopped"
      : a.pause && a.status !== "running" && a.status !== "exited" && a.status !== "disconnected"
        ? "paused"
        : a.status === "running" || a.status === "starting"
          ? "working"
          : a.status === "idle"
            ? "idle"
            : a.status === "disconnected"
              ? "offline"
              : "ended";
const stands = new Map<string, Stand>();

function lineOf(a: AgentInfo, s: Stand): string {
  const ap = a.approvals[0];
  if (s === "needs") return !ap ? "Waiting for you" : ap.tool === "question" ? `Asks: ${ap.title}` : ap.tool === "plan" ? "Proposes a plan" : `${ap.tool_name || ap.tool}: ${ap.title}`;
  if (s === "stopped") return `Stopped: ${a.exit?.error ?? "it stopped unexpectedly"}`;
  if (s === "paused") return "Paused: reached a usage limit";
  return `Finished: ${a.summary || a.goal || a.title || "Done"}`;
}

/** Every event the mock sends its pages: an agent that comes to stand somewhere new is told of, as kiln would. */
export function pushHeard(ev: unknown) {
  const e = ev as { type?: string; info?: AgentInfo };
  if (e.type !== "harness" || !e.info) return;
  const a = e.info;
  const s = standOf(a);
  const before = stands.get(a.id);
  stands.set(a.id, s);
  if (before === undefined || before === s || muted.has(a.id)) return;
  const kind: Kind | null = s === "needs" ? "agent.needs_you" : s === "stopped" ? "agent.stopped" : s === "paused" ? "agent.paused" : s === "idle" && before === "working" ? "agent.finished" : null;
  if (kind) wouldSend(kind, { title: a.name, body: lineOf(a, s), tag: a.id, url: `/agents/${a.id}` });
}

/* ---------------------------- a fleet of agents (MOCK_PUSH_FLEET=1) ---------------------------- */

const FLEET = process.env.MOCK_PUSH_FLEET === "1";
const t0 = Date.now();
const base = (id: string, name: string, target: string, kind: "claude" | "codex", ago: number): AgentInfo => ({
  ...defaultInfo(),
  id,
  kind,
  kind_name: kind === "claude" ? "Claude Code" : "Codex",
  target,
  name,
  cwd: "/home/ada/src/kiln",
  created_at: t0 - ago * 60_000,
  status: "idle",
  title: "",
  model: kind === "claude" ? "claude-opus-5-5" : "gpt-5.5-codex",
  mode: "default",
  approvals: [],
  queued: 0,
  tasks_running: 0,
  cost_usd: 0,
  last_activity: t0 - Math.min(ago, 3) * 60_000,
});
const fleet: AgentInfo[] = !FLEET
  ? []
  : [
      {
        ...base("fleet-db", "Pick the database", "agent:metal-ryzen-01", "claude", 40),
        status: "waiting",
        approvals: [{ id: "q1", tool: "question", tool_name: "", title: "Postgres or SQLite for the event store?", at: t0 }],
      },
      {
        ...base("fleet-tests", "Fix the flaky retry test", "agent:metal-ryzen-02", "claude", 35),
        status: "waiting",
        approvals: [{ id: "b1", tool: "shell", tool_name: "Bash", title: "cargo test -p kiln http::client", at: t0 }],
      },
      { ...base("fleet-docs", "Docs for reach", "local", "codex", 30), status: "running", activity: "Editing docs/reach.md", doing: "Editing docs/reach.md" },
      { ...base("fleet-ui", "Phone layout for Runs", "agent:metal-mac-mini", "claude", 25), status: "running", doing: "Running npx vite build" },
      {
        ...base("fleet-slates", "Slates", "agent:adas-macbook-pro-2.local", "claude", 26),
        status: "waiting",
        approvals: [{ id: "pp1", tool: "plan", tool_name: "ExitPlanMode", title: "Fix: a mount taken over by a restarted daemon dropped a store", at: t0 }],
      },
      {
        ...base("fleet-plan", "Plan QUIC replication", "agent:metal-mac-mini", "claude", 24),
        held: { why: 'Paused by the agent: plan "QUIC replication" is ready for you to look at: its steps go to the agent once you resume', plan: "pl_quic", since: t0 - 4 * 60_000 },
      },
      { ...base("fleet-crash", "Upgrade hyper-rt", "agent:metal-epyc-01", "codex", 22), status: "exited", exit: { code: 127, error: "codex: command not found" } },
      {
        ...base("fleet-limit", "Port the scheduler", "agent:metal-ryzen-01", "claude", 20),
        status: "idle",
        pause: { reason: "limit", label: "Session", said: "", since: t0, auto: true, by: "harness", tries: 0 },
      },
      { ...base("fleet-done", "Trim the release notes", "agent:metal-ryzen-02", "claude", 15), summary: "Trimmed the notes to the five changes that matter." },
      { ...base("fleet-bench", "Benchmark the codec", "agent:metal-epyc-01", "claude", 10), status: "running", doing: "Running cargo bench" },
    ];
/**
 * `MOCK_BUSY=N`: N more agents, all working, each saying what it does next every 0.7 s or so: a
 * fleet as busy as a real one, for measuring the pages under it.
 */
const BUSY = Number(process.env.MOCK_BUSY ?? 0);
const STEPS = ["Reading src/lib.rs", "Running cargo test", "Editing src/api.rs", "Thinking", "Running npm run build", "Searching for usages", "Writing tests", "Reviewing the diff"];
for (let i = 0; i < BUSY; i++) {
  fleet.push({ ...base(`busy-${i}`, `Busy agent ${i + 1}`, `agent:metal-ryzen-0${(i % 3) + 1}`, i % 2 ? "codex" : "claude", 9 - (i % 9)), status: "running", doing: STEPS[i % STEPS.length] });
}
/**
 * `MOCK_STANDS=paused:7,finished:16,offline:3,stopped:1`: that many more agents standing each of
 * those ways, for a fleet the size of a real one in Mission control's head.
 */
const TASKS = ["Port the scheduler", "Trim the release notes", "Docs for reach", "Fix the flaky retry test", "Phone layout for Runs", "Benchmark the codec"];
for (const [stand, n] of (process.env.MOCK_STANDS ?? "").split(",").filter(Boolean).map((s) => s.split(":"))) {
  for (let i = 0; i < Number(n); i++) {
    const a = base(`${stand}-${i}`, TASKS[i % TASKS.length]!, `agent:metal-ryzen-0${(i % 3) + 1}`, i % 2 ? "codex" : "claude", 50 + i);
    if (stand === "paused") fleet.push({ ...a, pause: { reason: "limit", label: "Session", said: "", since: t0, auto: true, by: "harness", tries: 0 } });
    else if (stand === "finished") fleet.push({ ...a, summary: "Done: the change is in and its tests pass." });
    else if (stand === "offline") fleet.push({ ...a, target: "agent:jupiter", status: "disconnected" });
    else if (stand === "stopped") fleet.push({ ...a, status: "exited", exit: { code: 1, error: "claude: the session ended unexpectedly" } });
  }
}
if (BUSY > 0)
  setInterval(() => {
    for (let i = 0; i < fleet.length; i++) {
      const a = fleet[i];
      if (!a?.id.startsWith("busy-") || Math.random() > 0.7) continue;
      const doing = STEPS[Math.floor(Math.random() * STEPS.length)];
      fleet[i] = { ...a, doing, activity: doing, last_activity: Date.now(), cost_usd: a.cost_usd + 0.002 };
      emitFleet(fleet[i]!);
    }
  }, 500);

/** The fleet's agents, beside the mock's own (`GET /api/harness/sessions`). */
export const fleetInfos = () => fleet;

let emitFleet: (info: AgentInfo) => void = () => {};
/** What Mission control's box sent (`GET /api/mock/broadcasts`). */
const broadcasts: unknown[] = [];
/**
 * A command to one of the fleet: answering a request takes it away (with none left it works on).
 * False: not one of the fleet.
 */
export function fleetCommand(id: string, c: { type?: string; id?: string; decision?: string }): boolean {
  const i = fleet.findIndex((a) => a.id === id);
  const was = fleet[i];
  if (!was) return false;
  if (c.type === "approve") {
    const approvals = was.approvals.filter((p) => p.id !== c.id);
    const changed: AgentInfo = { ...was, approvals, status: approvals.length > 0 ? "waiting" : "running", doing: approvals.length > 0 ? was.doing : c.decision === "deny" ? "Trying another way" : "Going ahead", last_activity: Date.now() };
    fleet[i] = changed;
    emitFleet(changed);
  }
  return true;
}

/* ---------------------------- /api/push ---------------------------- */

export async function handlePush(req: http.IncomingMessage, res: http.ServerResponse, url: URL, ctx: Ctx): Promise<boolean> {
  const p = url.pathname;
  const m = req.method ?? "GET";
  emitPush = () => ctx.emit({ type: "push" });
  emitFleet = (info) => ctx.emit({ type: "harness", info });
  if (p === "/api/mock/push/outbox" && m === "GET") return (ctx.json(res, 200, { sent: outbox }), true);
  // For trying Mission control: one of the fleet as it comes to stand otherwise (what changes, as
  // the agent's own fields), told to the pages as the server tells them.
  const fl = p.match(/^\/api\/mock\/fleet\/([^/]+)$/);
  if (fl && m === "POST") {
    const i = fleet.findIndex((a) => a.id === fl[1]);
    const was = fleet[i];
    if (!was) return (ctx.err(res, 404, "no agent of the fleet by that id"), true);
    const changed: AgentInfo = { ...was, ...(await ctx.body<Partial<AgentInfo>>(req)), last_activity: Date.now() };
    fleet[i] = changed;
    ctx.emit({ type: "harness", info: changed });
    return (ctx.json(res, 200, changed), true);
  }
  // Messages to many agents (Mission control's box): each kept, for trying it, and said sent.
  if (p === "/api/harness/files" && m === "POST") {
    let size = 0;
    for await (const chunk of req) size += (chunk as Buffer).length;
    return (ctx.json(res, 200, { file_id: `up-${randomBytes(6).toString("hex")}`, size }), true);
  }
  if (p === "/api/harness/broadcast" && m === "POST") {
    const b = await ctx.body<{ to?: { ids?: string[] }; text?: string; mode?: string; attachments?: unknown[]; command?: unknown; approvals?: string }>(req);
    broadcasts.push({ at: Date.now(), ...b });
    const ids = b.to?.ids ?? [];
    const results = ids.map((id) => {
      const a = fleet.find((x) => x.id === id);
      return { id, name: a?.name ?? id, machine: (a?.target ?? "local").replace(/^agent:/, ""), kind: a?.kind ?? "claude", result: "sent" };
    });
    return (ctx.json(res, 200, { id: `bc-${broadcasts.length}`, sent: results.length, skipped: 0, failed: 0, results }), true);
  }
  if (p === "/api/mock/broadcasts" && m === "GET") return (ctx.json(res, 200, { broadcasts }), true);
  if (!p.startsWith("/api/push")) return false;
  let mm: RegExpMatchArray | null;
  if (p === "/api/push" && m === "GET") {
    ctx.json(res, 200, { enabled: true, available: true, why: null, public_key: PUBLIC_KEY, kinds: KINDS, muted: [...muted], reached: { ok: true, error: null, checked_at: now() } });
    return true;
  }
  if (p === "/api/push/subscriptions" && m === "GET") return (ctx.json(res, 200, { devices: [...devices.values()] }), true);
  if (p === "/api/push/subscriptions" && m === "POST") {
    const b = await ctx.body<{ subscription?: { endpoint?: string; keys?: { p256dh?: string; auth?: string } }; device?: string; hears?: Kind[]; replaces?: string }>(req);
    const endpoint = b.subscription?.endpoint?.trim() ?? "";
    if (!/^https?:\/\//.test(endpoint) || !b.subscription?.keys?.p256dh || !b.subscription.keys.auth) return (ctx.err(res, 400, "a subscription needs its endpoint and keys"), true);
    const bad = (b.hears ?? []).find((k) => !KINDS.some((x) => x.id === k));
    if (bad) return (ctx.err(res, 400, `no kind of notification \`${bad}\``), true);
    const old = [...devices.values()].find((d) => d.endpoint === (b.replaces ?? endpoint));
    const d: Device = {
      id: old?.endpoint === endpoint ? old.id : randomBytes(8).toString("hex"),
      endpoint,
      device: b.device?.trim() || old?.device || "A device",
      hears: b.hears ? KINDS.map((k) => k.id).filter((k) => b.hears!.includes(k)) : (old?.hears ?? KINDS.map((k) => k.id)),
      created_at: old?.created_at ?? now(),
      last_ok_at: old?.last_ok_at ?? null,
      last_error: null,
      last_error_at: null,
    };
    if (old && old.id !== d.id) devices.delete(old.id);
    devices.set(d.id, d);
    emitPush();
    return (ctx.json(res, 201, d), true);
  }
  if ((mm = p.match(/^\/api\/push\/subscriptions\/([^/]+)$/)) && m === "DELETE") {
    const gone = devices.delete(decodeURIComponent(mm[1]!));
    emitPush();
    return (gone ? ctx.noContent(res) : ctx.err(res, 404, "no such device of yours"), true);
  }
  if (p === "/api/push/test" && m === "POST") {
    const b = await ctx.body<{ id?: string }>(req);
    const to = [...devices.values()].filter((d) => !b.id || d.id === b.id);
    if (to.length === 0) return (ctx.err(res, 404, "no device of yours gets notifications yet: turn them on in Settings first"), true);
    wouldSend("test", { title: "kiln", body: "Notifications reach this device.", tag: "kiln-test", url: "/settings" }, (d) => to.includes(d));
    return (ctx.json(res, 200, { sent: to.length, results: to.map((d) => ({ id: d.id, device: d.device, ok: true })) }), true);
  }
  if (p === "/api/push/notices" && m === "GET") return (ctx.json(res, 200, { you: "owner", marks: [...marks.values()].sort((a, b) => b.at - a.at) }), true);
  if (p === "/api/push/notices" && m === "POST") {
    const b = await ctx.body<{ keys?: string[]; how?: string; tags?: string[] }>(req);
    const keys = (b.keys ?? []).map((k) => k.trim()).filter(Boolean);
    if (b.how !== "away" && b.how !== "done" && b.how !== "seen") return (ctx.err(res, 400, "`how` is one of away, done, seen"), true);
    if (keys.length === 0) return (ctx.err(res, 400, "`keys`: one to 200 notices"), true);
    const at = Date.now();
    for (const key of keys) marks.set(key, { key, how: b.how, at: Math.max(at, marks.get(key)?.at ?? 0) });
    ctx.emit({ type: "notices", principal: "owner", keys, how: b.how, at, tags: b.tags ?? [] });
    return (ctx.json(res, 200, { marked: keys.length, at }), true);
  }
  if ((mm = p.match(/^\/api\/push\/mutes\/([^/]+)$/)) && (m === "PUT" || m === "DELETE")) {
    const agent = decodeURIComponent(mm[1]!);
    if (m === "PUT") muted.add(agent);
    else muted.delete(agent);
    emitPush();
    return (ctx.noContent(res), true);
  }
  return (ctx.err(res, 404, "no such route"), true);
}
