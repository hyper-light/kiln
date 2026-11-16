/**
 * Agent definitions in the mock, answered as the server's routes answer (crates/kiln/src/agentdefs;
 * kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, P5.1, P5.1b): a person's own, each with its versions, and
 * kiln's own beside them (`kiln:<name>`), the UI agents among them replaceable by one of yours.
 *
 *   GET    /api/agentdefs?all=           {agents: Brief[], reaches_all, builtin: Builtin[], uses}
 *   GET    /api/agentdefs/:id            one of yours, or one of kiln's (`kiln:<name>`)
 *   POST   /api/agentdefs                {name, harness, model?, instructions?, tools?, mcp?, skills?, limits?, agentfile?}: keep one
 *   PATCH  /api/agentdefs/:id            any of those: new settings are a new version, a name alone none
 *   GET    /api/agentdefs/:id/versions   {versions}, newest first
 *   POST   /api/agentdefs/:id/restore    {version}: that version's settings current again, as a new version
 *   DELETE /api/agentdefs/:id            it and its versions
 *   POST   /api/agentdefs/check          the settings, kept nowhere: {ok, problems}
 *   POST   /api/agentdefs/:id/run        {prompt?, on?, cwd?, name?}: an agent started from it
 *   POST   /api/agentdefs/:id/copy       {name?}: one of kiln's made yours
 *   POST   /api/agentdefs/use            {builtin, id | null}: one of yours stands in for one of kiln's UI agents
 *
 * Errors are `{error}`, with the server's codes and words. Times are Unix seconds.
 *
 * PROPOSALS, not the server's yet (p/uicontract is building them; they're here as the UI reads them):
 *   PATCH  /api/agentdefs/:id {version}  the version a save was based on: another saved since → 409 {error, current}
 *   GET    /api/agentdef-sessions        {[id]: {sessions: {id, version, variant?}[], starts: {n, by_kind, by_start, warm}}} (agentdefs.sessions for all, as uicontract serves it)
 *   GET    /api/agentdef-experiments     {[id]: Experiment[]}: each definition's experiments, splits of its new sessions between deployments (P7.1)
 */
import type http from "node:http";
import { createHash } from "node:crypto";

interface Ctx {
  json: (res: http.ServerResponse, status: number, body: unknown) => void;
  body: <T>(req: http.IncomingMessage) => Promise<T>;
}

const OWNER = "owner";
const BY = "admin";
const HARNESSES = ["claude", "codex", "devin", "gemini", "opencode", "goose", "amp", "antigravity", "cursor", "pi"];
const HARNESS_NAMES: Record<string, string> = {
  claude: "Claude Code",
  codex: "Codex",
  devin: "Devin",
  gemini: "Gemini",
  opencode: "OpenCode",
  goose: "Goose",
  amp: "Amp",
  antigravity: "Antigravity",
  cursor: "Cursor",
  pi: "Pi",
};
const NAME = /^[a-z0-9]+(?:[._-][a-z0-9]+)*$/;
const now = () => Math.floor(Date.now() / 1000);
/** One of yours' id, from its name: stable across the mock's reloads (the server's are its own). */
const idOf = (name: string) => createHash("sha256").update(`agentdef/${name}`).digest("hex").slice(0, 32);
/** An Agentfile of yours, as a definition names it (agentfiles.ts seeds them by these names). */
const agentfile = (name: string) => `agentfile:${createHash("sha256").update(name).digest("hex").slice(0, 32)}`;

type Mcp = { name: string; url?: string; command?: string[] };
type Limits = { seconds?: number; cents?: number; tokens?: number };
interface Settings {
  harness: string;
  model: string | null;
  instructions: string;
  tools: string[];
  mcp: Mcp[];
  skills: string[];
  limits: Limits;
  agentfile: string | null;
}
interface AgentDef extends Settings {
  id: string;
  owner: string;
  name: string;
  version: number;
  created_at: number;
  updated_at: number;
}
interface Version extends Settings {
  version: number;
  at: number;
  by: string;
}
interface Builtin extends Settings {
  id: string;
  name: string;
  about: string;
  replaceable: boolean;
  brief?: string;
}

const settings = (s: Partial<Settings> & { harness: string }): Settings => ({ model: null, instructions: "", tools: [], mcp: [], skills: [], limits: {}, agentfile: null, ...s });

/** kiln's own (crates/kiln/src/agentdefs/builtin.rs): a CI skill each, a harness each as it is, and the UI's agents. */
const BUILTIN: Builtin[] = [
  ...["author", "diagnose", "runners", "review", "migrate", "secrets", "performance"].map((n) => ({
    id: `kiln:kiln-ci-${n}`,
    name: `kiln-ci-${n}`,
    about: `kiln's kiln-ci-${n} skill, as an agent`,
    replaceable: false,
    ...settings({ harness: "claude", instructions: `Read skill://kiln/kiln-ci-${n}/SKILL.md and do what it says for what you're asked.`, skills: [`kiln-ci-${n}`] }),
  })),
  ...HARNESSES.map((h) => ({
    id: `kiln:harness-${h}`,
    name: `harness-${h}`,
    about: `${HARNESS_NAMES[h]} as it is, in kiln's Agentfile for it`,
    replaceable: false,
    ...settings({ harness: h, agentfile: `harness-${h}` }),
  })),
  ...(
    [
      ["kiln-about", 'The card that answers "ask about this code" in an agent\'s conversation: a child of that agent, reading and changing nothing'],
      ["kiln-about-code", 'The card that answers "ask about this code" in a change no agent made: kiln starts it with the repository at that commit'],
      ["kiln-attach", "An attachment's agent: woken by what it looks after (a failure, a stuck job, a mention, a schedule), it finds out what happened, acts within what it may do and reports"],
      ["kiln-review", "A review agent: an attachment asked to review a change, which comments, answers threads and gives its verdict, signed as itself"],
      ["kiln-toolfix", "The agent kiln starts on a runner to install a tool a job lacks there, then runs the job again"],
    ] as const
  ).map(([name, about]) => ({ id: `kiln:${name}`, name, about, replaceable: true, brief: `${about}.`, ...settings({ harness: "claude" }) })),
];

const kept = new Map<string, AgentDef>();
const versions = new Map<string, Version[]>();
let uses: Record<string, string> = {};

function seed() {
  if (kept.size) return;
  const day = 86_400;
  const put = (name: string, at: number, ...steps: Partial<Settings>[]) => {
    const first = settings({ harness: "claude", ...steps[0] });
    const d: AgentDef = { id: idOf(name), owner: OWNER, name, version: 1, created_at: at, updated_at: at, ...first };
    const vs: Version[] = [{ version: 1, at, by: BY, ...first }];
    steps.slice(1).forEach((step, i) => {
      const next = { ...settingsOf(d), ...step };
      Object.assign(d, next, { version: i + 2, updated_at: at + (i + 1) * 3600 });
      vs.unshift({ version: d.version, at: d.updated_at, by: BY, ...next });
    });
    kept.set(d.id, d);
    versions.set(d.id, vs);
    return d;
  };
  put(
    "reviewer",
    now() - 6 * day,
    { harness: "claude", model: "claude-opus-5-5", instructions: "Review the change.", agentfile: agentfile("rust-dev"), tools: ["read", "grep"], skills: ["code-review"] },
    { instructions: "Review the change against AGENTS.md." },
    { limits: { seconds: 45 * 60 } },
    {
      instructions: "Review the change against AGENTS.md; findings with file and line.",
      tools: ["read", "grep", "bash"],
      mcp: [{ name: "github", url: "https://api.githubcopilot.com/mcp/" }],
      skills: ["code-review", "security-review", "simplify"],
    },
  );
  put(
    "pr-triage",
    now() - 4 * day,
    { harness: "codex", model: "gpt-6.1-sol", instructions: "Label and route new pull requests.", agentfile: agentfile("rust-dev"), tools: ["read"] },
    { model: "gpt-6.1-sol-mini", limits: { seconds: 10 * 60 } },
  );
  put("release-notes", now() - 3 * day, { harness: "gemini", instructions: "Notes from landed diffs.", agentfile: agentfile("docs-site") });
  const terse = put("about-terse", now() - 2 * day, { harness: "claude", model: "claude-sonnet-5-5", instructions: "Explain the code in three lines.", tools: ["read"], limits: { seconds: 120 } });
  uses = { "kiln:kiln-about": terse.id };
}

const settingsOf = (d: Settings): Settings => ({
  harness: d.harness,
  model: d.model,
  instructions: d.instructions,
  tools: d.tools,
  mcp: d.mcp,
  skills: d.skills,
  limits: d.limits,
  agentfile: d.agentfile,
});
const brief = (d: AgentDef) => ({
  id: d.id,
  owner: d.owner,
  name: d.name,
  harness: d.harness,
  model: d.model,
  agentfile: d.agentfile,
  version: d.version,
  created_at: d.created_at,
  updated_at: d.updated_at,
});
const same = (a: Settings, b: Settings) => JSON.stringify(settingsOf(a)) === JSON.stringify(settingsOf(b));

/** What `check` says of settings, in the server's words (agentdefs/mod.rs check). */
function problems(s: Partial<Settings>): string[] {
  if (!s.harness || !HARNESSES.includes(s.harness)) return [`\`${s.harness ?? ""}\` isn't one of kiln's harnesses: ${HARNESSES.join(", ")}`];
  const out: string[] = [];
  if (s.model === "") out.push("`model` names a model, or is left out for its harness's default");
  const mcp = s.mcp ?? [];
  if (mcp.some((m) => !m.name)) out.push("an MCP server needs its name");
  for (const m of mcp) if (!m.url === !(m.command && m.command.length)) out.push(`MCP server ${m.name} is reached by its \`url\` or started by its \`command\`: one of them`);
  if ((s.tools ?? []).some((t) => !t)) out.push("a tool needs its name");
  if ((s.skills ?? []).some((t) => !t)) out.push("a skill needs its name");
  for (const k of ["seconds", "cents", "tokens"] as const) if (s.limits?.[k] === 0) out.push(`a limit of 0 ${k} lets it do nothing: leave it out for none`);
  if (s.agentfile && !/^agentfile:[0-9a-f]{32}$/.test(s.agentfile) && s.agentfile !== `harness-${s.harness}`)
    out.push(`\`image\` is kiln's Agentfile for its harness (\`harness-${s.harness}\`) or one of yours (\`agentfile:<id>\`), not ${JSON.stringify(s.agentfile)}`);
  return out;
}

/** The fields a create or update may give, read as the server reads them: `null` resets only `model` and `agentfile`. */
function given(b: Record<string, unknown>): Partial<Settings> | string {
  const out: Partial<Settings> = {};
  for (const [k, v] of Object.entries(b)) {
    if (k === "name" || k === "version") continue;
    if (v === null && k !== "model" && k !== "agentfile") return `\`${k}\`: invalid type: null`;
    if (["harness", "instructions"].includes(k) && typeof v !== "string") return `\`${k}\`: invalid type: expected a string`;
    if (["model", "agentfile"].includes(k) && v !== null && typeof v !== "string") return `\`${k}\`: invalid type: expected a string`;
    if (["tools", "skills"].includes(k) && !(Array.isArray(v) && v.every((x) => typeof x === "string"))) return `\`${k}\`: invalid type: expected a sequence of strings`;
    if (k === "mcp" && !Array.isArray(v)) return "`mcp`: invalid type: expected a sequence";
    if (k === "limits" && (typeof v !== "object" || v === null)) return "`limits`: invalid type: expected a map";
    if (["harness", "model", "instructions", "tools", "mcp", "skills", "limits", "agentfile"].includes(k)) (out as Record<string, unknown>)[k] = v;
  }
  return out;
}

/* ---------------------------------------- proposals: sessions per definition, their starts, experiments */

/** A measure's median day by day: settling toward where it stands now with a day's noise, the same each run (its seed). */
const trendOf = (mid: number, spread: number, seed: number, days = 14) => {
  let x = seed * 9301 + 49297;
  const rnd = () => (x = (x * 9301 + 49297) % 233280) / 233280;
  const out: number[] = [];
  let v = mid + spread * (rnd() - 0.2);
  for (let i = 0; i < days; i++) {
    v += (mid - v) * 0.3 + (rnd() - 0.5) * spread * 0.4;
    out.push(Math.round(v * 100) / 100);
  }
  out[days - 1] = mid;
  return out;
};
const variant = (
  id: string,
  agentfileName: string,
  cpus: number,
  memGiB: number,
  weight: number,
  sessions: number,
  warm: [number, number, number],
  peak: [number, number, number],
  failed: [number, number, number],
  current = false,
  name: string | null = null,
) => {
  const seed = [...id].reduce((h, c) => (h * 31 + c.charCodeAt(0)) % 9973, 11);
  return {
    id,
    name,
    agentfile: agentfileName,
    engine: "microvm" as const,
    cpus,
    memory: memGiB * 2 ** 30,
    weight,
    sessions,
    current,
    measures: { warm_ms: warm, peak_gib: peak, failed_pct: failed },
    trend: { warm_ms: trendOf(warm[1], warm[2] - warm[0], seed), peak_gib: trendOf(peak[1], peak[2] - peak[0], seed + 1), failed_pct: trendOf(failed[1], failed[2] - failed[0], seed + 2) },
  };
};
const DAY = 86_400_000;
const METRIC = { warm_ms: "kiln.session.warm_ms", peak_gib: "kiln.session.peak_memory_gib", failed_pct: "kiln.session.failed_pct" } as const;
/** A criterion as its person wrote it: a monitor query over one of kiln's session metrics, by variant, and its bound. */
const criterion = (experiment: string, id: string, name: string, measure: keyof typeof METRIC, op: string, bound: number) => ({
  id,
  name,
  query: `avg(last_14d):avg:${METRIC[measure]}{experiment:${experiment}} by {variant}`,
  op,
  bound,
});
/** Each definition's experiments, newest first, several at once on one (P7.1): a proposal until the server has experiments. */
const EXPERIMENTS: Record<string, unknown[]> = {
  [idOf("reviewer")]: [
    {
      id: "reviewer-size",
      name: "Right-size its microVM",
      state: "live",
      version: 1,
      sessions: 96,
      started_at: Date.now() - 6 * DAY,
      criteria: [
        criterion("reviewer-size", "warm", "Warm start", "warm_ms", "<=", 250),
        criterion("reviewer-size", "failed", "Failed sessions", "failed_pct", "<", 10),
        criterion("reviewer-size", "peak", "Peak memory", "peak_gib", "<", 3.5),
      ],
      variants: [
        variant("5e1c", "rust-dev", 2, 4, 50, 49, [208, 215, 223], [2.24, 2.29, 2.35], [6, 12, 20], true, "Small"),
        variant("a37d", "rust-dev", 4, 8, 50, 47, [212, 221, 229], [2.94, 3.0, 3.07], [0, 2, 8], false, "Large"),
      ],
    },
    {
      id: "reviewer-image",
      name: "rust-dev v9's image",
      state: "paused",
      version: 2,
      sessions: 31,
      started_at: Date.now() - 12 * DAY,
      criteria: [criterion("reviewer-image", "failed", "Failed sessions", "failed_pct", "<", 8), criterion("reviewer-image", "warm", "Warm start", "warm_ms", "<=", 260)],
      variants: [
        variant("b10c", "rust-dev", 2, 4, 70, 22, [206, 214, 222], [2.2, 2.27, 2.33], [4, 9, 16], true, "v8 image"),
        variant("b20d", "rust-dev-v9", 2, 4, 30, 9, [198, 207, 219], [2.1, 2.18, 2.27], [1, 5, 14], false, "v9 image"),
      ],
    },
  ],
  [idOf("pr-triage")]: [
    {
      id: "triage-sizes",
      name: "Six sizes",
      state: "live",
      version: 3,
      sessions: 214,
      started_at: Date.now() - 9 * DAY,
      criteria: [
        criterion("triage-sizes", "failed", "Failed sessions", "failed_pct", "<", 10),
        criterion("triage-sizes", "peak", "Peak memory", "peak_gib", "<=", 3),
        criterion("triage-sizes", "warm", "Warm start", "warm_ms", "<", 240),
      ],
      variants: [
        variant("0b9e", "rust-dev", 1, 2, 20, 44, [196, 204, 213], [1.71, 1.76, 1.82], [8, 15, 24], true),
        variant("c4a1", "rust-dev", 2, 2, 20, 41, [199, 207, 216], [1.74, 1.79, 1.85], [5, 11, 19]),
        variant("7f20", "rust-dev", 2, 4, 20, 43, [201, 209, 218], [2.21, 2.27, 2.33], [1, 4, 10]),
        variant("e853", "rust-dev", 4, 4, 15, 31, [204, 212, 221], [2.26, 2.31, 2.38], [0, 3, 10]),
        variant("19d6", "rust-dev", 4, 8, 15, 33, [207, 216, 226], [2.93, 2.99, 3.06], [0, 2, 9]),
        variant("d2fb", "docs-site", 2, 4, 10, 22, [188, 199, 211], [1.95, 2.02, 2.1], [2, 8, 21]),
      ],
    },
    {
      id: "triage-docs",
      name: "Docs image for docs PRs",
      state: "promoted",
      version: 1,
      sessions: 58,
      started_at: Date.now() - 20 * DAY,
      criteria: [criterion("triage-docs", "failed", "Failed sessions", "failed_pct", "<", 10)],
      variants: [
        variant("d001", "rust-dev", 2, 4, 0, 29, [201, 210, 219], [2.2, 2.26, 2.32], [6, 11, 18]),
        variant("d002", "docs-site", 2, 4, 100, 29, [186, 195, 205], [1.9, 1.96, 2.03], [2, 5, 11], true),
      ],
    },
  ],
};
/** How fast each starts where, as its data is kept here: the engine its sessions run in, its warm hosts, each kind of start's median (ms). */
type StartsKept = { engine: "microvm" | "host"; warm: number; target: number; p50_ms: { warm?: number; template?: number; cold?: number } };
const starts = (engine: "microvm" | "host", warm: number, target: number, p50_ms: StartsKept["p50_ms"]): StartsKept => ({ engine, warm, target, p50_ms });
/** Shards' word for each kind of start (its D253: `booted` cold, `restored` from its image's template, `warm` from a warm host). */
const WORD: Record<string, string> = { cold: "booted", template: "restored", warm: "warm" };
/**
 * Its starts as the server answers them (agentdefs.sessions' `starts`, crates/kiln/src/agentdefs/sessions.rs): how many
 * started, by engine (median in ms), by how each started (median in µs, and Shards' words for it), and its warm hosts
 * (a proposal: the server says none until its runners' Hex nodes report them).
 */
function served(k: StartsKept, sessions: number) {
  const kinds = Object.entries(k.p50_ms).filter((e): e is [string, number] => e[1] !== undefined);
  const each = Math.max(1, Math.round(sessions / Math.max(1, kinds.length)));
  const sorted = kinds.map(([, ms]) => ms).sort((a, b) => a - b);
  const mid = sorted.length ? sorted[Math.floor(sorted.length / 2)]! : null;
  return {
    n: sessions,
    by_kind: sessions ? { [k.engine]: { n: sessions, timed: sessions, p50_ms: mid } } : {},
    by_start: Object.fromEntries(kinds.map(([kind, ms]) => [kind, { n: each, timed: each, p50_us: Math.round(ms * 1000), from: { [WORD[kind] ?? kind]: each } }])),
    warm: k.target > 0 ? { warm: k.warm, target: k.target } : null,
  };
}
/** The sessions started from each, the version each started on and its variant, and how fast it starts where: a proposal (p/uicontract). */
const SESSIONS: Record<string, { sessions: { id: string; version: number; variant?: string }[]; starts: StartsKept }> = {
  [idOf("reviewer")]: {
    sessions: [
      { id: "fam-orch", version: 4, variant: "5e1c" },
      { id: "fan-lead", version: 4, variant: "a37d" },
      { id: "fan-s2", version: 4, variant: "5e1c" },
      { id: "fam-migr", version: 4, variant: "a37d" },
      { id: "rev-big", version: 3, variant: "5e1c" },
    ],
    starts: starts("microvm", 4, 7, { warm: 214, template: 438, cold: 1890 }),
  },
  [idOf("pr-triage")]: {
    sessions: [
      { id: "sub-codex", version: 2, variant: "0b9e" },
      { id: "fam-tests", version: 2, variant: "7f20" },
      { id: "fan-s3", version: 2, variant: "c4a1" },
    ],
    starts: starts("microvm", 0, 1, { warm: 205, template: 455, cold: 1940 }),
  },
  [idOf("release-notes")]: {
    sessions: [
      { id: "sub-busy", version: 1 },
      { id: "sub-done", version: 1 },
    ],
    starts: starts("microvm", 0, 0, { warm: 198 }),
  },
  [idOf("about-terse")]: { sessions: [], starts: starts("microvm", 0, 0, {}) },
  "kiln:kiln-ci-diagnose": { sessions: [{ id: "att-ci", version: 0 }], starts: starts("microvm", 1, 1, { warm: 230, cold: 1912 }) },
  "kiln:harness-claude": { sessions: [], starts: starts("microvm", 0, 0, { template: 612, cold: 1884 }) },
  "kiln:harness-codex": { sessions: [], starts: starts("microvm", 0, 0, { template: 640, cold: 1930 }) },
  "kiln:harness-devin": { sessions: [], starts: starts("microvm", 0, 0, { cold: 2104 }) },
};

/** The definition a session started from and the deployment its variant gave it: what its microVM was sized and built from (microvms.ts). */
export function sessionDef(session: string): { agent: string; agentfile: string; cpus: number; memory: number } | null {
  seed();
  for (const [id, s] of Object.entries(SESSIONS)) {
    const mine = s.sessions.find((x) => x.id === session);
    if (!mine) continue;
    const d = kept.get(id);
    const name = d?.name ?? BUILTIN.find((b) => b.id === id)?.name ?? id;
    const exps = (EXPERIMENTS[id] ?? []) as { variants: { id: string; agentfile: string; cpus: number; memory: number }[] }[];
    const v = mine.variant ? exps.flatMap((e) => e.variants).find((x) => x.id === mine.variant) : undefined;
    const af = v?.agentfile ?? (d?.agentfile?.startsWith("agentfile:") ? (name === "release-notes" ? "docs-site" : "rust-dev") : `harness-${d?.harness ?? "claude"}`);
    return { agent: name.replace(/^kiln-ci-/, "ci-"), agentfile: af, cpus: v?.cpus ?? 2, memory: v?.memory ?? 4 * 2 ** 30 };
  }
  return null;
}

/* ---------------------------------------- the routes */

/** Handles the routes above; false when it doesn't know the route. */
export async function handleAgentdefs(req: http.IncomingMessage, res: http.ServerResponse, url: URL, ctx: Ctx): Promise<boolean> {
  const p = url.pathname;
  const m = req.method ?? "GET";
  if (p === "/api/agentdef-sessions" && m === "GET")
    return (seed(), ctx.json(res, 200, Object.fromEntries(Object.entries(SESSIONS).map(([id, d]) => [id, { sessions: d.sessions, starts: served(d.starts, d.sessions.length) }]))), true);
  if (p === "/api/agentdef-experiments" && m === "GET") return (seed(), ctx.json(res, 200, EXPERIMENTS), true);
  if (p !== "/api/agentdefs" && !p.startsWith("/api/agentdefs/")) return false;
  seed();
  const fail = (status: number, error: string) => (ctx.json(res, status, { error }), true);
  const answer = (body: unknown) => (ctx.json(res, 200, body), true);
  const mine = (id: string) => kept.get(id);
  const kilns = (id: string) => (id.startsWith("kiln:") ? `${id} is kiln's own agent definition: copy it as yours (agentdefs.copy) to change it` : null);

  if (p === "/api/agentdefs" && m === "GET") {
    const agents = [...kept.values()].sort((a, b) => b.updated_at - a.updated_at || a.id.localeCompare(b.id)).map(brief);
    return answer({ agents, reaches_all: true, builtin: BUILTIN, uses });
  }
  if (p === "/api/agentdefs" && m === "POST") {
    const b = await ctx.body<Record<string, unknown>>(req);
    const name = typeof b.name === "string" ? b.name.trim() : "";
    if (!name) return fail(400, "`name` is required");
    if (!NAME.test(name) || name.length > 255) return fail(400, `\`${name}\` isn't an agent's name: lowercase letters and digits joined by \`.\`, \`_\` or \`-\` (\`reviewer\`), 255 at the most`);
    if (typeof b.harness !== "string") return fail(400, "`harness` is required");
    const g = given(b);
    if (typeof g === "string") return fail(400, g);
    const s = settings({ harness: b.harness, ...g });
    const why = problems(s);
    if (why.length) return fail(422, `kiln can't run this agent: ${why.join("; ")}`);
    if ([...kept.values()].some((d) => d.name === name)) return fail(409, `you have an agent named ${name} already: give this one another name`);
    const d: AgentDef = { id: idOf(`${name}/${now()}`), owner: OWNER, name, version: 1, created_at: now(), updated_at: now(), ...s };
    kept.set(d.id, d);
    versions.set(d.id, [{ version: 1, at: d.created_at, by: BY, ...s }]);
    return answer(d);
  }
  if (p === "/api/agentdefs/check" && m === "POST") {
    const b = await ctx.body<Record<string, unknown>>(req);
    if (typeof b.harness !== "string") return fail(400, "`harness` is required");
    const g = given(b);
    if (typeof g === "string") return fail(400, g);
    const why = problems({ ...g, harness: b.harness });
    return answer({ ok: why.length === 0, problems: why });
  }
  if (p === "/api/agentdefs/use" && m === "POST") {
    const b = await ctx.body<{ builtin?: string; id?: string | null }>(req);
    if (!b.builtin) return fail(400, "`builtin` is required");
    const bi = BUILTIN.find((x) => x.id === b.builtin);
    if (!bi) return fail(400, `${b.builtin} isn't one of kiln's agent definitions (\`kiln:<name>\`, as agentdefs.list's \`builtin\` names them)`);
    if (!bi.replaceable)
      return fail(
        400,
        `${b.builtin} isn't one kiln starts for you that yours can stand in for: ${BUILTIN.filter((x) => x.replaceable)
          .map((x) => x.id)
          .join(", ")}`,
      );
    if (b.id && !mine(b.id)) return fail(404, "no such agent definition");
    const next = { ...uses };
    if (b.id) next[bi.id] = b.id;
    else delete next[bi.id];
    uses = next;
    return answer({ uses });
  }
  const [, , , rawId, rest] = p.split("/");
  const id = decodeURIComponent(rawId ?? "");
  if (id.startsWith("kiln:")) {
    const bi = BUILTIN.find((x) => x.id === id);
    if (!bi) return fail(403, `${id} isn't one of kiln's agent definitions`);
    if (!rest && m === "GET") return answer(bi);
    if (rest === "copy" && m === "POST") {
      const b = await ctx.body<{ name?: string }>(req);
      const name = (b.name ?? bi.name).trim();
      if ([...kept.values()].some((d) => d.name === name)) return fail(409, `you have an agent named ${name} already: give this one another name`);
      const d: AgentDef = { id: idOf(`${name}/${now()}`), owner: OWNER, name, version: 1, created_at: now(), updated_at: now(), ...settingsOf(bi) };
      kept.set(d.id, d);
      versions.set(d.id, [{ version: 1, at: d.created_at, by: BY, ...settingsOf(bi) }]);
      return answer(d);
    }
    if (rest === "run" && m === "POST") return answer({ agents: [], parent: null, queued: "the mock starts no agent of kiln's own", agentdef: { id, version: 0 } });
    return fail(403, kilns(id)!);
  }
  const d = mine(id);
  if (!d) return fail(404, "no such agent definition");
  if (!rest && m === "GET") return answer(d);
  if (rest === "versions" && m === "GET") return answer({ versions: versions.get(d.id) ?? [] });
  if (!rest && m === "DELETE") {
    kept.delete(d.id);
    versions.delete(d.id);
    uses = Object.fromEntries(Object.entries(uses).filter(([, v]) => v !== d.id));
    return answer({ deleted: true });
  }
  if (!rest && m === "PATCH") {
    const b = await ctx.body<Record<string, unknown>>(req);
    // PROPOSAL (p/uicontract): a save names the version it was based on; another saved since, refused with the current one.
    if (typeof b.version === "number" && b.version !== d.version) return (ctx.json(res, 409, { error: "it changed since you read it: read it again, then change it", current: d }), true);
    const g = given(b);
    if (typeof g === "string") return fail(400, g);
    const name = typeof b.name === "string" ? b.name.trim() : d.name;
    if (name !== d.name && !NAME.test(name)) return fail(400, `\`${name}\` isn't an agent's name: lowercase letters and digits joined by \`.\`, \`_\` or \`-\` (\`reviewer\`), 255 at the most`);
    if (name !== d.name && [...kept.values()].some((x) => x.name === name)) return fail(409, `you have an agent named ${name} already: give this one another name`);
    const next = { ...settingsOf(d), ...g };
    const why = problems(next);
    if (why.length) return fail(422, `kiln can't run this agent: ${why.join("; ")}`);
    if (same(next, d) && name === d.name) return answer(d);
    if (!same(next, d)) {
      Object.assign(d, next, { version: d.version + 1, updated_at: now() });
      versions.get(d.id)?.unshift({ version: d.version, at: d.updated_at, by: BY, ...next });
    }
    d.name = name;
    d.updated_at = now();
    return answer(d);
  }
  if (rest === "restore" && m === "POST") {
    const b = await ctx.body<{ version?: number | string }>(req);
    const v = Number(b.version);
    if (!(v >= 1)) return fail(400, "`version` is the version to make current again");
    const was = (versions.get(d.id) ?? []).find((x) => x.version === v);
    if (!was) return fail(400, `the agent definition has no version ${v}`);
    if (same(was, d)) return answer(d);
    Object.assign(d, settingsOf(was), { version: d.version + 1, updated_at: now() });
    versions.get(d.id)?.unshift({ version: d.version, at: d.updated_at, by: BY, ...settingsOf(was) });
    return answer(d);
  }
  if (rest === "copy" && m === "POST") return fail(404, "no such agent definition");
  if (rest === "run" && m === "POST")
    return answer({
      agents: [],
      parent: null,
      queued: `the mock started no agent: ${d.name} would start as ${HARNESS_NAMES[d.harness] ?? d.harness} in a microVM`,
      agentdef: { id: d.id, version: d.version },
    });
  return fail(404, `no route ${m} ${p}`);
}
