/**
 * Goal stacks in the mock (docs/goals.md), answered as the server's goals routes answer: a stack per
 * agent, kept here. "Release prep" (/agents/fam-orch) has one like the one a person reported: a goal
 * skipped, one under way, four to come. What the server sends the agent is played as sent at once
 * (`push`): its goal under way, or the next. No reminders, no verdicts.
 *
 *   GET  /api/harness/sessions/:id/goals                 the stack
 *   POST /api/harness/sessions/:id/goals                 {goals, start?}: goals after the others
 *   PUT  /api/harness/sessions/:id/goals                 {goals: [{id?, text}]}: the goals still to go
 *   POST /api/harness/sessions/:id/goals/:action         start, pause, skip, clear, restore
 */
import type http from "node:http";
import type { GoalItem, GoalStack } from "../src/api/harness.ts";

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

interface Kept {
  state: GoalStack["state"];
  reason?: string;
  items: GoalItem[];
  updated_at: number;
  /** The goals the last clear kept. */
  cleared?: GoalItem[];
}

const PAUSED_BY_YOU = "Paused by you: the agent isn't held to its goal (its own goal is cleared) until you resume.";
let seq = 0;
const newId = () => `g-${Date.now().toString(36)}-${(seq++).toString(36)}`;
const words = (t: string) => t.split(/\s+/).filter(Boolean).join(" ");
const toGo = (i: GoalItem) => i.status === "pending" || i.status === "active";

const t0 = Date.now() - 3 * 3600_000;
const stacks = new Map<string, Kept>([
  [
    "fam-orch",
    {
      state: "running",
      updated_at: Date.now() - 60_000,
      items: [
        {
          id: "g-beat",
          text: "Beat Splinter DB and RocksDB by minimum 4-5x on fills, gets, and seeks without any sacrifice in correctness or robustness.",
          status: "skipped",
          reason: "Skipped by you.",
          how: "goal",
          started_at: t0,
          ended_at: t0 + 3600_000,
        },
        {
          id: "g-quic",
          text: "Add full hyperaraft dual UDP/QUIC stack replication to our metadata store without impacting the 4-5x lead on Splinter and RocksDB",
          status: "active",
          how: "goal",
          started_at: t0 + 3600_000,
          reminded_at: Date.now() - 4 * 60_000,
        },
        {
          id: "g-store",
          text: "Build the data storage/transfer layer and replication using hyper-raft and its crates. We should be transferring data via hyper-quic internally even if our front-end is through the HTTP/1.1 API. Robustly test locally by actually transferring 1k/10k/100k small files and 10k/100k 10GB-100GB files (both streamed single upload and parts). Ensure they replicate correctly given their cells and metadata.",
          status: "pending",
        },
        {
          id: "g-opt",
          text: "Optimize the QUIC API and s3 HTTP API using the same benchmarking and profiling we have in steps 1-3, minimizing allocs, reallocs, latency, excess memory, and page faults. DO NOT sacrifice ANY ounce of correctness or robustness.",
          status: "pending",
        },
        { id: "g-plat", text: "Validate and perform steps 1-3 if not already validated for Linux, Windows, Mac arm64 and amd64/x86_64.", status: "pending" },
        { id: "g-ci", text: "Build the CI/CD pipelines and publish our single binary for Windows, Linux, MacOS, arm64/amd64/x86_64.", status: "pending" },
      ],
    },
  ],
]);

function stackOf(agent: string): Kept {
  let s = stacks.get(agent);
  if (!s) {
    s = { state: "paused", items: [], updated_at: 0 };
    stacks.set(agent, s);
  }
  return s;
}

function view(agent: string, s: Kept): GoalStack {
  return { agent, state: s.state, reason: s.reason, items: s.items, updated_at: s.updated_at, ...(s.cleared?.length ? { cleared: s.cleared.length } : {}) };
}

/** What's next goes to the agent now: the goal under way (sent again), else the next one. */
function push(s: Kept) {
  if (s.state !== "running") return;
  const now = Date.now();
  const active = s.items.find((i) => i.status === "active");
  if (active) {
    if (!active.reminded_at) Object.assign(active, { reminded_at: now, reminders: undefined });
    return;
  }
  const next = s.items.find((i) => i.status === "pending");
  if (next) Object.assign(next, { status: "active", how: "goal", started_at: now, reminded_at: now });
  else s.state = "done";
}

/** The goals still to go, as given (as the server's `set` takes them). Returns whether the goal under way changed. */
function set(s: Kept, given: { id?: string; text: string }[]): boolean {
  const byId = new Map(s.items.map((i) => [i.id, i]));
  const seen = new Set<string>();
  const list: { id?: string; text: string }[] = [];
  for (const g of given) {
    const text = words(g.text);
    if (!text) continue;
    const i = g.id && !seen.has(g.id) ? byId.get(g.id) : undefined;
    if (i && !toGo(i)) continue;
    if (i) seen.add(i.id);
    list.push({ id: i?.id, text });
  }
  const done = s.items.filter((i) => !toGo(i));
  const active = s.items.find((i) => i.status === "active");
  const at = active ? list.findIndex((g) => g.id === active.id) : -1;
  let moved = false;
  const out: GoalItem[] = [...done];
  if (active && at <= 0) {
    if (at === 0 && words(active.text) !== list[0]!.text) {
      Object.assign(active, { text: list[0]!.text, reminded_at: undefined, reminders: undefined });
      moved = true;
    }
    out.push(active);
  }
  list.forEach((g, k) => {
    if (active && g.id === active.id) {
      if (k > 0) {
        Object.assign(active, { status: "pending", text: g.text, how: undefined, started_at: undefined, reminded_at: undefined, reminders: undefined });
        out.push(active);
        moved = true;
      }
      return;
    }
    const i = g.id ? byId.get(g.id) : undefined;
    out.push(i ? Object.assign(i, { text: g.text }) : { id: newId(), text: g.text, status: "pending" });
  });
  s.items = out;
  if (s.state === "done" && s.items.some(toGo)) Object.assign(s, { state: "paused", reason: undefined });
  return moved;
}

function control(s: Kept, action: string): string | null {
  const now = Date.now();
  const active = s.items.find((i) => i.status === "active");
  switch (action) {
    case "start":
      if (active) Object.assign(active, { reminded_at: undefined, reminders: undefined });
      Object.assign(s, { state: s.items.some(toGo) ? "running" : "done", reason: undefined });
      push(s);
      return null;
    case "pause":
      Object.assign(s, { state: "paused", reason: PAUSED_BY_YOU });
      return null;
    case "skip":
      if (active) Object.assign(active, { status: "skipped", ended_at: now, reason: "Skipped by you." });
      push(s);
      return null;
    case "clear":
      if (s.items.length) s.cleared = s.items;
      Object.assign(s, { items: [], state: "paused", reason: undefined });
      return null;
    case "restore": {
      const back = s.cleared ?? [];
      if (!back.length) return "no goals were cleared here to bring back";
      const busy = Boolean(active);
      const restored = back.map((i) =>
        i.status !== "active" ? i : busy ? { ...i, status: "pending" as const, how: undefined, started_at: undefined, reminded_at: undefined } : { ...i, reminded_at: undefined, reminders: undefined },
      );
      s.items = [
        ...restored.filter((i) => !toGo(i)),
        ...s.items.filter((i) => !toGo(i)),
        ...s.items.filter((i) => i.status === "active"),
        ...restored.filter(toGo),
        ...s.items.filter((i) => i.status === "pending"),
      ];
      Object.assign(s, { cleared: undefined, state: s.items.some(toGo) ? "running" : "done", reason: undefined });
      push(s);
      return null;
    }
    default:
      return "not found";
  }
}

export async function handleGoals(req: http.IncomingMessage, res: http.ServerResponse, url: URL): Promise<boolean> {
  const m = /^\/api\/harness\/sessions\/([^/]+)\/goals(?:\/([a-z]+))?$/.exec(url.pathname);
  if (!m) return false;
  const agent = decodeURIComponent(m[1]!);
  const action = m[2];
  const method = req.method ?? "GET";
  const s = stackOf(agent);
  if (!action && method === "GET") {
    json(res, 200, view(agent, s));
    return true;
  }
  if (!action && method === "POST") {
    const b = await body<{ goals?: string[]; start?: boolean }>(req);
    const texts = (b.goals ?? []).map(words).filter(Boolean);
    if (!texts.length) {
      json(res, 400, { error: "`goals` is a list of at least one goal" });
      return true;
    }
    set(s, [...s.items.filter((i) => i.status === "pending").map((i) => ({ id: i.id, text: i.text })), ...texts.map((text) => ({ text }))]);
    if (b.start) control(s, "start");
  } else if (!action && method === "PUT") {
    const b = await body<{ goals?: { id?: string; text: string }[] }>(req);
    if (set(s, b.goals ?? [])) push(s);
  } else if (action && method === "POST") {
    const why = control(s, action);
    if (why) {
      json(res, why === "not found" ? 404 : 400, { error: why });
      return true;
    }
  } else {
    return false;
  }
  s.updated_at = Date.now();
  json(res, 200, view(agent, s));
  return true;
}
