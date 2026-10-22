/**
 * Programs in the mock (docs/programs.md), answered as the server's routes answer: the critical
 * path, ten tracks and 120 goals (six workers: four at work on a goal the coordinator handed them,
 * Hex T7 paused mid-goal at its session limit, Isolation T4 between goals), its account at 87% of
 * the weekly window (no new goal starts, so the track next for the free slot is held), and a
 * small second program. The person's settings and assignments are kept here, and each track's slot
 * and threshold follow them as the server's do.
 *
 *   GET /api/programs                   every program, the critical path first
 *   GET /api/programs/:id               one
 *   PUT /api/programs/:id/settings      {slots?, weights?}: the person's
 *   PUT /api/programs/:id/tracks/:t     {agent}: a track's worker (null: none)
 */
import type http from "node:http";
import type { Program, ProgramAgent, ProgramChange, ProgramNotice, ProgramPause, ProgramSettings, ProgramTrack, TrackSlot } from "../src/api/programs.ts";

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

interface WorkerSeed {
  name: string;
  status: string;
  pct: number;
  pause?: ProgramPause;
  /** A threshold a person set (else kiln's). */
  pause_at?: number;
}

interface TrackSeed {
  n: number;
  name: string;
  weight: number;
  goals: number;
  met: number;
  worker?: WorkerSeed;
  /** Its worker has no goal of the coordinator's under way (between two). */
  between?: boolean;
  changes?: ProgramChange[];
}

interface ProgramSeed {
  id: string;
  title: string;
  tag: string;
  coordinator: ProgramAgent;
  prefix: string;
  tracks: TrackSeed[];
}

const W = (name: string, status: string, pct: number, more: Partial<WorkerSeed> = {}): WorkerSeed => ({ name, status, pct, ...more });
const CR = (change: string, state: string, checks: string | undefined, branch: string, title: string): ProgramChange => ({ change, state, checks, branch, title, url: `https://kiln.example/review/${change}` });
const HOUR = 3_600_000;
const started = Date.now();

const SEEDS: ProgramSeed[] = [
  {
    id: "01a157fddb8276a2996246ea931c4808",
    title: "Critical path, master: Amphora, Quiver, sends never lost or doubled, isolated agents, Hex, Veil, Reliquary and Mantle, in ten tracks",
    tag: "critical-path",
    coordinator: { id: "fam-orch", name: "Kiln Distro", status: "running", stack: "running", usage: { pct: 87, account: "ada@kiln.example" } },
    prefix: "CP",
    tracks: [
      { n: 1, name: "Amphora", weight: 5, goals: 13, met: 3, worker: W("Amphora", "running", 64), changes: [CR("CR52", "built", "passed", "cp/t1-engine", "Amphora's engine: group commit")] },
      { n: 2, name: "Quiver", weight: 3, goals: 13, met: 1 },
      { n: 3, name: "Sends and session records", weight: 5, goals: 7, met: 2, worker: W("Sends T3", "running", 71), changes: [CR("CR48", "merged", "passed", "cp/t3-journal", "The session journal, durable before acknowledged")] },
      { n: 4, name: "Isolation", weight: 4, goals: 10, met: 1, worker: W("Isolation T4", "idle", 58), between: true },
      { n: 5, name: "UIs", weight: 2, goals: 10, met: 0 },
      { n: 6, name: "Quality and observability", weight: 3, goals: 6, met: 1, worker: W("Quality T6", "running", 66, { pause_at: 90 }), changes: [CR("CR50", "built", "failed", "cp/t6-sim", "kiln-sim, and the bugs it finds")] },
      {
        n: 7,
        name: "Hex",
        weight: 4,
        goals: 14,
        met: 1,
        worker: W("Hex T7", "idle", 69, { pause: { reason: "limit", window: "five_hour", label: "Session", until: started + 2 * HOUR } }),
        changes: [CR("CR47", "merged", "passed", "cp/t7-hex", "Hex set up: process and oci classes")],
      },
      { n: 8, name: "Veil", weight: 3, goals: 10, met: 0 },
      { n: 9, name: "Reliquary", weight: 3, goals: 10, met: 0 },
      { n: 10, name: "Mantle in kiln", weight: 2, goals: 27, met: 2, worker: W("Mantle T10", "running", 62) },
    ],
  },
  {
    id: "plan-docs-master",
    title: "Docs refresh, master: guides, the API reference and examples, in three tracks",
    tag: "docs",
    coordinator: { id: "fam-docs", name: "Docs lead", status: "idle", stack: "running" },
    prefix: "DR",
    tracks: [
      { n: 1, name: "Guides", weight: 2, goals: 3, met: 2, worker: W("Guides T1", "running", 22) },
      { n: 2, name: "API reference", weight: 1, goals: 3, met: 0 },
      { n: 3, name: "Examples", weight: 1, goals: 2, met: 2 },
    ],
  },
];

const workerId = (seed: ProgramSeed, n: number) => `${seed.id.slice(0, 12)}-t${n}`;

/** The person's settings, and the tracks' workers, by program. */
const kept = new Map<string, ProgramSettings>();
const assigned = new Map<string, Record<string, string>>();

function settingsOf(seed: ProgramSeed): ProgramSettings {
  return kept.get(seed.id) ?? { slots: null, weights: Object.fromEntries(seed.tracks.map((t) => [`T${t.n}`, t.weight])), seeded: true, updated_at: 0, by: "" };
}
function workersOf(seed: ProgramSeed): Record<string, string> {
  return assigned.get(seed.id) ?? Object.fromEntries(seed.tracks.filter((t) => t.worker).map((t) => [`T${t.n}`, workerId(seed, t.n)]));
}

/** Each track's slot, as the server's rotation gives it (programs::rotate). */
function rotate(tracks: { n: number; weight: number; pass: number; running: boolean; done: boolean }[], slots: number): TrackSlot[] {
  const running = tracks.filter((t) => t.running).length;
  const free = Math.max(0, slots - running);
  const ready = tracks.filter((t) => !t.running && !t.done).sort((a, b) => a.pass - b.pass || b.weight - a.weight || a.n - b.n);
  const next = new Set(ready.slice(0, free).map((t) => t.n));
  return tracks.map((t) => (t.running ? "running" : t.done ? "done" : next.has(t.n) ? "next" : "waiting"));
}

function build(seed: ProgramSeed): Program {
  const settings = settingsOf(seed);
  const workers = workersOf(seed);
  const slots = settings.slots ?? 6;
  const parts = seed.tracks.map((t) => {
    const id = `T${t.n}`;
    const weight = settings.weights[id] ?? 1;
    const agent = workers[id];
    // The seed's worker when it's the one assigned; another agent kiln knows nothing more of.
    const w = agent && agent === workerId(seed, t.n) ? t.worker : undefined;
    const done = t.met >= t.goals;
    const underWay = !!agent && !done && !(w && t.between);
    const paused = !!w?.pause;
    // At work or paused mid-goal, it holds its slot.
    return { seed: t, id, weight, agent, w, done, underWay, paused, running: underWay, pass: t.met / weight };
  });
  const slotsOf = rotate(parts.map((p) => ({ n: p.seed.n, weight: p.weight, pass: p.pass, running: p.running, done: p.done })), slots);
  const lowest = parts.filter((p) => p.running).sort((a, b) => a.weight - b.weight || b.pass - a.pass || b.seed.n - a.seed.n)[0]?.seed.n;
  const pcts = [seed.coordinator.usage?.pct, ...parts.map((p) => p.w?.pct)].filter((x): x is number => typeof x === "number");
  const pct = pcts.length ? Math.max(...pcts) : null;
  const rule = pct === null || pct <= 80 ? { rule: "ok" as const } : pct > 95 ? { rule: "pause" as const, why: `Paused for usage: the weekly window is at ${pct}%` } : { rule: "hold" as const, why: `Held for usage: the weekly window is at ${pct}%` };
  const tracks: ProgramTrack[] = parts.map((p, i) => {
    const t = p.seed;
    const key = (m: number) => `${seed.prefix}-T${t.n}.${m}`;
    const wants = p.agent ? (lowest === t.n ? 80 : 95) : undefined;
    const worker: ProgramAgent | null = p.agent
      ? {
          id: p.agent,
          name: p.w?.name ?? `agent ${p.agent.slice(0, 8)}`,
          status: p.w?.status ?? "idle",
          ...(p.w?.pause && { pause: p.w.pause }),
          pause_at: p.w?.pause_at ?? wants,
          ...(p.w && { usage: { pct: p.w.pct, account: "ada@kiln.example" } }),
        }
      : null;
    const next = t.met + 1;
    const slot = slotsOf[i] ?? "waiting";
    return {
      id: p.id,
      name: t.name,
      plan: `${seed.id}-plan-t${t.n}`,
      weight: p.weight,
      worker,
      ...(p.agent && { assigned: p.agent }),
      goal: p.done
        ? null
        : {
            id: `${seed.id}-goal-${t.n}-${next}`,
            key: key(next),
            text: `${key(next)} ${t.name}: step ${next} of ${t.goals} — read the T${t.n} plan, then work it until it's met`,
            status: "pending",
            ...(p.underWay && worker && { on: worker.id, on_name: worker.name }),
          },
      met: t.met,
      total: t.goals,
      pass: p.pass,
      slot: p.paused && p.underWay ? "paused" : slot === "next" && rule.rule !== "ok" ? "held" : slot,
      ...(wants && { wants }),
      changes: t.changes ?? [],
    };
  });
  return {
    id: seed.id,
    title: seed.title,
    tag: seed.tag,
    coordinator: seed.coordinator,
    coordinator_id: seed.coordinator.id,
    slots,
    running: tracks.filter((t) => t.slot === "running").length,
    settings,
    usage: pct === null ? null : { pct, account: "ada@kiln.example" },
    rule,
    met: tracks.reduce((n, t) => n + t.met, 0),
    total: tracks.reduce((n, t) => n + t.total, 0),
    goal: { id: `${seed.id}-goal-m3`, text: `${seed.prefix}-M3 Work this goal stack's track goals in parallel, and a Programs view to follow them`, status: "active" },
    tracks,
  };
}

/** What changed, as the coordinator's told it: delivered at once (its session takes the message). */
function notice(seed: ProgramSeed, said: string[]): ProgramNotice | undefined {
  if (said.length === 0) return undefined;
  return { text: `The person changed ${seed.title}'s settings: ${said.join(", ")}.`, at: Date.now(), delivered: true, message: `msg-${Date.now().toString(36)}` };
}

export async function handlePrograms(req: http.IncomingMessage, res: http.ServerResponse, url: URL): Promise<boolean> {
  const parts = url.pathname.split("/").filter(Boolean);
  if (parts[0] !== "api" || parts[1] !== "programs") return false;
  if (req.method === "GET" && parts.length === 2) {
    json(res, 200, { programs: SEEDS.map(build) });
    return true;
  }
  const seed = SEEDS.find((s) => s.id === decodeURIComponent(parts[2] ?? ""));
  if (!seed) {
    json(res, 404, { error: `no program ${parts[2]}` });
    return true;
  }
  if (req.method === "GET" && parts.length === 3) {
    json(res, 200, build(seed));
    return true;
  }
  if (req.method === "PUT" && parts.length === 4 && parts[3] === "settings") {
    const b = await body<{ slots?: number; weights?: Record<string, number> }>(req);
    const before = build(seed);
    const s: ProgramSettings = structuredClone(settingsOf(seed));
    const said: string[] = [];
    if (b.slots !== undefined) {
      if (!Number.isInteger(b.slots) || b.slots < 1 || b.slots > 32) {
        json(res, 400, { error: "`slots` is a whole number from 1 to 32" });
        return true;
      }
      if (b.slots !== before.slots) said.push(`slots ${before.slots} → ${b.slots}`);
      s.slots = b.slots;
    }
    for (const [t, w] of Object.entries(b.weights ?? {})) {
      if (!seed.tracks.some((x) => `T${x.n}` === t)) {
        json(res, 400, { error: `${seed.id} has no track ${t}` });
        return true;
      }
      if (!Number.isInteger(w) || w < 1 || w > 100) {
        json(res, 400, { error: `${t}'s weight is a whole number from 1 to 100` });
        return true;
      }
      const was = s.weights[t] ?? 1;
      if (was !== w) said.push(`${t}'s weight ${was} → ${w}`);
      s.weights[t] = w;
    }
    s.updated_at = Date.now();
    s.by = "admin";
    s.notice = notice(seed, said) ?? s.notice;
    kept.set(seed.id, s);
    json(res, 200, build(seed));
    return true;
  }
  if (req.method === "PUT" && parts.length === 5 && parts[3] === "tracks") {
    const track = decodeURIComponent(parts[4] ?? "");
    if (!seed.tracks.some((x) => `T${x.n}` === track)) {
      json(res, 400, { error: `${seed.id} has no track ${track}` });
      return true;
    }
    const b = await body<{ agent?: string | null }>(req);
    const w = { ...workersOf(seed) };
    if (b.agent === null || b.agent === "") delete w[track];
    else if (typeof b.agent === "string") w[track] = b.agent;
    assigned.set(seed.id, w);
    json(res, 200, build(seed));
    return true;
  }
  return false;
}
