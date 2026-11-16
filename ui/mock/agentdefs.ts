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
 * Its experiments (`agentdefs.experiments.*`, crates/kiln/src/agentdefs/experiments; kiln plan
 * 01a1cd8e862e7169a074db3d5fa33f3d P7.1), each a split of a definition's new sessions between deployments, one
 * live at a time on a definition; kiln's own take none (403) and another person's is as none (404):
 *   GET    /api/agentdef-experiments?id=&all=          {[id]: Experiment[]}, each definition's newest first
 *   POST   /api/agentdefs/:id/experiments              {name, variants: {weight, name?, engine?, agentfile?, cpus?, memory?, model?}[], criteria?}: live (409 beside another live, 422 a variant kiln can't run)
 *   PATCH  /api/agentdefs/:id/experiments/:e           {name}
 *   PATCH  …/:e/variants/:v                            {name?}: none, back to what sets it apart
 *   POST   …/:e/weights                                {weights: {[variant]: percent}, base?}: its split's next version where one changed (409 over a base moved on)
 *   POST   …/:e/variants, DELETE …/:e/variants/:v      a deployment added at weight 0; one taken out, its share back to the rest in proportion
 *   POST   …/:e/criteria, DELETE …/:e/criteria/:c      {name, query, op, bound}, as kiln's monitors read them
 *   POST   …/:e/pause, …/resume, …/stop                (409 once it's over, or resuming beside another live)
 *   POST   …/:e/promote                                {variant}: its deployment the definition's next version, the split's weights kept → {experiment, agentdef}
 *   DELETE …/:e                                        {deleted: true}
 *   POST   /api/mock/agentdef-experiments              the seeded experiments again (the browser tests start from them)
 * Each change answers {experiment}. Its seeded variants' `measures` and `trend` are a PROPOSAL (the server's next
 * part of P7.1): one made here has none, as the server's have.
 *
 * Errors are `{error}`, with the server's codes and words. Times are Unix seconds, an experiment's Unix ms.
 *
 * PROPOSALS, not the server's yet (p/uicontract is building them; they're here as the UI reads them):
 *   PATCH  /api/agentdefs/:id {version}  the version a save was based on: another saved since → 409 {error, current}
 *   GET    /api/agentdef-sessions        {[id]: {sessions: {id, version, variant?}[], starts: {n, by_kind, by_start, warm}}} (agentdefs.sessions for all, as uicontract serves it)
 */
import type http from "node:http";
import { createHash, randomBytes } from "node:crypto";
import { checkMonitor } from "./observe-check.ts";

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
  /** Where its sessions run: left out, a microVM (an experiment's promotion sets these, as the server's does). */
  engine?: "host";
  /** A microVM's vCPUs and memory (bytes); left out, kiln sizes it. */
  cpus?: number;
  memory?: number;
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
  ...(d.engine ? { engine: d.engine } : {}),
  ...(d.cpus !== undefined ? { cpus: d.cpus } : {}),
  ...(d.memory !== undefined ? { memory: d.memory } : {}),
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

/* ---------------------------------------- experiments (P7.1), and proposals: sessions per definition, their starts */

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
type Interval = [number, number, number];
type Op = ">" | ">=" | "==" | "!=" | "<" | "<=";
const OPS: readonly Op[] = [">", ">=", "==", "!=", "<", "<="];
/** A variant as kept: a deployment and its share of new sessions (whole percents, together 100). */
interface Variant {
  id: string;
  name: string | null;
  engine: "microvm" | "host";
  /** A microVM's: `agentfile:<id>` (one of its owner's) or `harness-<harness>`; none on the host. */
  agentfile: string | null;
  cpus?: number;
  /** Bytes. */
  memory?: number;
  model?: string;
  weight: number;
  /** How many of its draws' sessions started. */
  sessions: number;
  /** PROPOSAL (the server's next part of P7.1): what its sessions measured, the seeded variants' alone. */
  measures?: { warm_ms: Interval; peak_gib: Interval; failed_pct: Interval };
  trend?: { warm_ms: number[]; peak_gib: number[]; failed_pct: number[] };
}
interface Criterion {
  id: string;
  name: string;
  query: string;
  op: Op;
  bound: number;
}
interface Experiment {
  id: string;
  agentdef: string;
  name: string;
  state: "live" | "paused" | "promoted" | "stopped";
  /** Unix ms, as `started_at`. */
  state_at: number;
  /** Its split's version: one more with each change of its weights. */
  version: number;
  started_at: number;
  promoted?: string;
  by: string;
  criteria: Criterion[];
  variants: Variant[];
  /** The sessions its removed variants were drawn for: still its own. */
  dropped: number;
}
const variant = (
  id: string,
  agentfileName: string,
  cpus: number,
  memGiB: number,
  weight: number,
  sessions: number,
  warm: Interval,
  peak: Interval,
  failed: Interval,
  name: string | null = null,
): Variant => {
  const seed = [...id].reduce((h, c) => (h * 31 + c.charCodeAt(0)) % 9973, 11);
  return {
    id,
    name,
    engine: "microvm",
    agentfile: agentfile(agentfileName),
    cpus,
    memory: memGiB * 2 ** 30,
    weight,
    sessions,
    measures: { warm_ms: warm, peak_gib: peak, failed_pct: failed },
    trend: { warm_ms: trendOf(warm[1], warm[2] - warm[0], seed), peak_gib: trendOf(peak[1], peak[2] - peak[0], seed + 1), failed_pct: trendOf(failed[1], failed[2] - failed[0], seed + 2) },
  };
};
/** The seeded variants' Agentfiles by name, for the microVMs made of them (microvms.ts keys its images by name). */
const AGENTFILE_NAMES = new Map(["rust-dev", "rust-dev-v9", "docs-site"].map((n) => [agentfile(n), n]));
const DAY = 86_400_000;
const MIB = 2 ** 20;
const METRIC = { warm_ms: "kiln.session.warm_ms", peak_gib: "kiln.session.peak_memory_gib", failed_pct: "kiln.session.failed_pct" } as const;
/** A criterion as its person wrote it: a monitor query over one of kiln's session metrics, by variant, and its bound. */
const criterion = (experiment: string, id: string, name: string, measure: keyof typeof METRIC, op: Op, bound: number): Criterion => ({
  id,
  name,
  query: `avg(last_14d):avg:${METRIC[measure]}{experiment:${experiment}} by {variant}`,
  op,
  bound,
});
/** The definitions' experiments as the mock starts (and `POST /api/mock/agentdef-experiments` puts back), several on one, one of them live. */
function seededExperiments(): Experiment[] {
  const at = Date.now();
  const [reviewer, triage] = [idOf("reviewer"), idOf("pr-triage")];
  return [
    {
      id: "reviewer-size",
      agentdef: reviewer,
      name: "Right-size its microVM",
      state: "live",
      state_at: at - 6 * DAY,
      version: 1,
      started_at: at - 6 * DAY,
      by: BY,
      dropped: 0,
      criteria: [
        criterion("reviewer-size", "warm", "Warm start", "warm_ms", "<=", 250),
        criterion("reviewer-size", "failed", "Failed sessions", "failed_pct", "<", 10),
        criterion("reviewer-size", "peak", "Peak memory", "peak_gib", "<", 3.5),
      ],
      variants: [
        variant("5e1c", "rust-dev", 2, 4, 50, 49, [208, 215, 223], [2.24, 2.29, 2.35], [6, 12, 20], "Small"),
        variant("a37d", "rust-dev", 4, 8, 50, 47, [212, 221, 229], [2.94, 3.0, 3.07], [0, 2, 8], "Large"),
      ],
    },
    {
      id: "reviewer-image",
      agentdef: reviewer,
      name: "rust-dev v9's image",
      state: "paused",
      state_at: at - 2 * DAY,
      version: 2,
      started_at: at - 12 * DAY,
      by: BY,
      dropped: 0,
      criteria: [criterion("reviewer-image", "failed", "Failed sessions", "failed_pct", "<", 8), criterion("reviewer-image", "warm", "Warm start", "warm_ms", "<=", 260)],
      variants: [
        variant("b10c", "rust-dev", 2, 4, 70, 22, [206, 214, 222], [2.2, 2.27, 2.33], [4, 9, 16], "v8 image"),
        variant("b20d", "rust-dev-v9", 2, 4, 30, 9, [198, 207, 219], [2.1, 2.18, 2.27], [1, 5, 14], "v9 image"),
      ],
    },
    {
      id: "triage-sizes",
      agentdef: triage,
      name: "Six sizes",
      state: "live",
      state_at: at - 9 * DAY,
      version: 3,
      started_at: at - 9 * DAY,
      by: BY,
      dropped: 0,
      criteria: [
        criterion("triage-sizes", "failed", "Failed sessions", "failed_pct", "<", 10),
        criterion("triage-sizes", "peak", "Peak memory", "peak_gib", "<=", 3),
        criterion("triage-sizes", "warm", "Warm start", "warm_ms", "<", 240),
      ],
      variants: [
        variant("0b9e", "rust-dev", 1, 2, 20, 44, [196, 204, 213], [1.71, 1.76, 1.82], [8, 15, 24]),
        variant("c4a1", "rust-dev", 2, 2, 20, 41, [199, 207, 216], [1.74, 1.79, 1.85], [5, 11, 19]),
        variant("7f20", "rust-dev", 2, 4, 20, 43, [201, 209, 218], [2.21, 2.27, 2.33], [1, 4, 10]),
        variant("e853", "rust-dev", 4, 4, 15, 31, [204, 212, 221], [2.26, 2.31, 2.38], [0, 3, 10]),
        variant("19d6", "rust-dev", 4, 8, 15, 33, [207, 216, 226], [2.93, 2.99, 3.06], [0, 2, 9]),
        variant("d2fb", "docs-site", 2, 4, 10, 22, [188, 199, 211], [1.95, 2.02, 2.1], [2, 8, 21]),
      ],
    },
    {
      id: "triage-docs",
      agentdef: triage,
      name: "Docs image for docs PRs",
      state: "promoted",
      state_at: at - 14 * DAY,
      version: 1,
      started_at: at - 20 * DAY,
      promoted: "d002",
      by: BY,
      dropped: 0,
      criteria: [criterion("triage-docs", "failed", "Failed sessions", "failed_pct", "<", 10)],
      variants: [
        variant("d001", "rust-dev", 2, 4, 0, 29, [201, 210, 219], [2.2, 2.26, 2.32], [6, 11, 18]),
        variant("d002", "docs-site", 2, 4, 100, 29, [186, 195, 205], [1.9, 1.96, 2.03], [2, 5, 11]),
      ],
    },
  ];
}
let keptExperiments: Experiment[] | null = null;
const experiments = () => (keptExperiments ??= seededExperiments());
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
    const v = mine.variant
      ? experiments()
          .filter((e) => e.agentdef === id)
          .flatMap((e) => e.variants)
          .find((x) => x.id === mine.variant)
      : undefined;
    const its = d?.agentfile?.startsWith("agentfile:") ? (name === "release-notes" ? "docs-site" : "rust-dev") : `harness-${d?.harness ?? "claude"}`;
    // A variant names its Agentfile by id; the microVMs here know their images by name.
    const af = v?.agentfile ? (AGENTFILE_NAMES.get(v.agentfile) ?? v.agentfile) : its;
    return { agent: name.replace(/^kiln-ci-/, "ci-"), agentfile: af, cpus: v?.cpus ?? 2, memory: v?.memory ?? 4 * 2 ** 30 };
  }
  return null;
}

/* ---------------------------------------- experiments' operations, as the server's (crates/kiln/src/agentdefs/experiments) */

/** An experiment as its operations answer it: each variant whether it's what its definition runs now, the sessions it drew. */
function experimentOf(e: Experiment) {
  const current = e.promoted ?? e.variants[0]?.id;
  return {
    id: e.id,
    agentdef: e.agentdef,
    name: e.name,
    state: e.state,
    state_at: e.state_at,
    version: e.version,
    sessions: e.dropped + e.variants.reduce((n, v) => n + v.sessions, 0),
    started_at: e.started_at,
    ...(e.promoted ? { promoted: e.promoted } : {}),
    by: e.by,
    criteria: e.criteria.map((c) => ({ ...c })),
    variants: e.variants.map((v) => ({
      id: v.id,
      name: v.name,
      engine: v.engine,
      agentfile: v.agentfile,
      ...(v.cpus !== undefined ? { cpus: v.cpus } : {}),
      ...(v.memory !== undefined ? { memory: v.memory } : {}),
      ...(v.model ? { model: v.model } : {}),
      weight: v.weight,
      sessions: v.sessions,
      current: v.id === current,
      ...(v.measures ? { measures: v.measures, trend: v.trend } : {}),
    })),
  };
}

/** By definition, each one's newest first. */
function listed(of: (e: Experiment) => boolean) {
  const out: Record<string, ReturnType<typeof experimentOf>[]> = {};
  const all = experiments()
    .filter(of)
    .sort((a, b) => b.started_at - a.started_at || b.id.localeCompare(a.id));
  for (const e of all) (out[e.agentdef] ??= []).push(experimentOf(e));
  return out;
}

/** A refusal: its status and its words, as the server's `fail` gives them. */
type No = { status: number; error: string };
const no = (status: number, error: string): No => ({ status, error });
const isNo = (x: unknown): x is No => typeof x === "object" && x !== null && "status" in x && "error" in x;

/** A field's type as serde reads it: one of its words where it's an enum's. */
type Kind = "string" | "u32" | "u64" | "i64" | "f64" | "list" | "percents" | readonly string[];
/** An input's fields (`deny_unknown_fields`), each its type and whether it's required. */
type Shape = Record<string, [Kind, boolean?]>;
const ENGINES = ["host", "microvm"] as const;
const ROUTE: Shape = { id: ["string", true], experiment: ["string", true] };
/** What each operation takes (ops.rs `input`), its route's own (`id`, `experiment` …) among them. */
const INPUTS: Record<string, Shape> = {
  create: { id: ["string", true], name: ["string", true], variants: ["list", true], criteria: ["list"] },
  rename: { ...ROUTE, name: ["string"], variant: ["string"] },
  weights: { ...ROUTE, weights: ["percents", true], base: ["i64"] },
  "variants.add": { ...ROUTE, name: ["string"], engine: [ENGINES], agentfile: ["string"], cpus: ["u32"], memory: ["u64"], model: ["string"] },
  "variants.remove": { ...ROUTE, variant: ["string", true] },
  "criteria.add": { ...ROUTE, name: ["string", true], query: ["string", true], op: [OPS, true], bound: ["f64", true] },
  "criteria.remove": { ...ROUTE, criterion: ["string", true] },
  pause: ROUTE,
  resume: ROUTE,
  stop: ROUTE,
  promote: { ...ROUTE, variant: ["string", true] },
  delete: ROUTE,
};
const VARIANT_INPUT: Shape = { name: ["string"], weight: ["u32", true], engine: [ENGINES], agentfile: ["string"], cpus: ["u32"], memory: ["u64"], model: ["string"] };
const CRITERION_INPUT: Shape = { name: ["string", true], query: ["string", true], op: [OPS, true], bound: ["f64", true] };

const isObject = (v: unknown): v is Record<string, unknown> => typeof v === "object" && v !== null && !Array.isArray(v);
const isWhole = (v: unknown, most: number): v is number => typeof v === "number" && Number.isInteger(v) && v >= 0 && v <= most;
const U32 = 2 ** 32 - 1;
function fits(v: unknown, kind: Kind): boolean {
  if (typeof kind !== "string") return typeof v === "string" && kind.includes(v);
  if (kind === "string") return typeof v === "string";
  if (kind === "f64") return typeof v === "number";
  if (kind === "i64") return Number.isInteger(v);
  if (kind === "list") return Array.isArray(v);
  if (kind === "percents") return isObject(v) && Object.values(v).every((w) => isWhole(w, U32));
  return isWhole(v, kind === "u32" ? U32 : Number.MAX_SAFE_INTEGER);
}
/** `o` read as an input of `shape`, as serde reads one: a field it doesn't take, one it needs left out, or one of another type refused (400), saying which. */
function read(o: unknown, shape: Shape, what: string): No | null {
  if (!isObject(o)) return no(400, `invalid type: expected ${what}`);
  const fields = Object.keys(shape);
  const stray = Object.keys(o).find((k) => !fields.includes(k));
  if (stray) return no(400, `unknown field \`${stray}\`, expected one of ${fields.map((f) => `\`${f}\``).join(", ")}`);
  for (const [k, [kind, required]] of Object.entries(shape)) {
    const v = o[k];
    if (v === undefined || v === null) {
      if (required) return no(400, `missing field \`${k}\``);
    } else if (!fits(v, kind)) {
      return no(
        400,
        typeof kind === "string" ? `\`${k}\`: invalid type: ${JSON.stringify(v)}, expected ${kind}` : `unknown variant \`${String(v)}\`, expected one of ${kind.map((x) => `\`${x}\``).join(", ")}`,
      );
    }
  }
  return null;
}

/** `text` as a name: trimmed, one line; none where it's empty. */
function line(text: unknown, what: string): string | null | No {
  const t = typeof text === "string" ? text.trim() : "";
  // eslint-disable-next-line no-control-regex
  if (/[\u0000-\u001f\u007f]/.test(t)) return no(400, `${what} is one line of text`);
  return t || null;
}

/** Whether `weights` are a split: each a whole percent, together 100. */
function whole(weights: number[]): No | null {
  const over = weights.find((w) => w > 100);
  if (over !== undefined) return no(400, `a weight is a whole percent of new sessions, 100 at most: not ${over}`);
  const sum = weights.reduce((a, w) => a + w, 0);
  return sum === 100 ? null : no(400, `a split's weights are whole percents that add up to 100, not ${sum}`);
}

/** `parts` scaled in proportion to add up to 100, each whole: the largest remainder method (kiln_ci::split::rescale); none for nothing. */
function rescale(parts: number[]): number[] | null {
  const sum = parts.reduce((a, x) => a + x, 0);
  if (sum === 0) return null;
  const out = parts.map((p) => Math.floor((p * 100) / sum));
  const left = 100 - out.reduce((a, x) => a + x, 0);
  const order = parts.map((p, k) => [(p * 100) % sum, k] as const).sort((a, b) => b[0] - a[0] || a[1] - b[1]);
  for (const [, k] of order.slice(0, left)) out[k] = (out[k] ?? 0) + 1;
  return out;
}

/** A variant in a few words, for what kiln says of it: its name, else its deployment. */
const shown = (v: Variant) => v.name ?? (v.engine === "host" ? "the host" : `${v.agentfile ?? "its Agentfile"} with ${v.cpus ?? 0} vCPU and ${Math.floor((v.memory ?? 0) / MIB)} MiB`);
const oneLive = (name: string) => no(409, `${name} is live on this agent, and one at a time splits its sessions: pause, stop or promote it first`);
const over = (e: Experiment) => no(409, `${e.name} is ${e.state}: it's over`);
const running = (e: Experiment) => e.state === "live" || e.state === "paused";
const liveBeside = (def: string, except: string) => experiments().find((x) => x.agentdef === def && x.state === "live" && x.id !== except);

/** What the definition's check says of an Agentfile its owner names (lineage.rs `image_for`): one of yours, or kiln's for its harness. */
function imageProblem(d: AgentDef, asked: string): string | null {
  const name = asked.trim();
  if (name.startsWith("agentfile:")) return /^agentfile:[0-9a-f]{32}$/.test(name) ? null : `${name} names no Agentfile of yours`;
  const of = name.startsWith("harness-") ? name.slice("harness-".length) : null;
  if (of && HARNESSES.includes(of)) return of === d.harness ? null : `${name} is kiln's Agentfile for ${of}, not ${HARNESS_NAMES[d.harness] ?? d.harness}`;
  return `\`image\` is kiln's Agentfile for its harness (\`harness-<harness>\`) or one of yours (\`agentfile:<id>\`), not ${JSON.stringify(name)}`;
}

/**
 * The deployment variant `ask` asks of definition `d`, said to be `which` in what kiln says of it (experiments' `deployment`):
 * a microVM by default, its Agentfile and size the definition's where it gives none; the host with neither. Checked as the
 * definition would be, run so: refused (422), every reason said.
 */
function deployment(d: AgentDef, ask: Record<string, unknown>, which: string): Omit<Variant, "id" | "weight" | "sessions"> | No {
  const refused = (why: string[]) => no(422, `kiln can't run this split: ${why.map((w) => `${which}: ${w}`).join("; ")}`);
  const name = line(ask.name, "a variant's name");
  if (isNo(name)) return name;
  const model = typeof ask.model === "string" ? ask.model.trim() : undefined;
  if (model === "") return refused(["`model` names a model, or is left out for its agent's"]);
  const engine = ask.engine === "host" ? "host" : "microvm";
  const given = { agentfile: typeof ask.agentfile === "string" ? ask.agentfile.trim() : undefined, cpus: ask.cpus as number | undefined, memory: ask.memory as number | undefined };
  const s =
    engine === "host"
      ? given
      : { agentfile: given.agentfile ?? d.agentfile ?? (HARNESSES.includes(d.harness) ? `harness-${d.harness}` : undefined), cpus: given.cpus ?? d.cpus, memory: given.memory ?? d.memory };
  if (engine === "microvm") {
    const unsaid: string[] = [];
    if (s.agentfile === undefined) unsaid.push(`kiln ships no Agentfile for ${d.harness}: name one of yours (\`agentfile\`)`);
    if (s.cpus === undefined) unsaid.push("say its microVM's vCPUs (`cpus`)");
    if (s.memory === undefined) unsaid.push("say its microVM's memory, in bytes (`memory`)");
    if (unsaid.length) return refused(unsaid);
  }
  const why: string[] = [];
  if (s.agentfile !== undefined) {
    const p = imageProblem(d, s.agentfile);
    if (p) why.push(p);
  }
  if (engine === "host" && (s.agentfile !== undefined || s.cpus !== undefined || s.memory !== undefined))
    why.push("an Agentfile and a size are a microVM's: on the host engine its harness runs as a process of its machine");
  if (s.cpus === 0) why.push("a microVM of 0 vCPUs runs nothing: leave `cpus` out for kiln to size it");
  if (s.memory === 0) why.push("a microVM of no memory runs nothing: leave `memory` out for kiln to size it");
  else if (s.memory !== undefined && s.memory % MIB !== 0) why.push(`\`memory\` is in bytes, a whole number of MiB (${MIB} bytes each), not ${s.memory}`);
  if (why.length) return refused(why);
  return engine === "host"
    ? { name, engine, agentfile: null, ...(model ? { model } : {}) }
    : { name, engine, agentfile: s.agentfile ?? null, cpus: s.cpus, memory: s.memory, ...(model ? { model } : {}) };
}

/** `ask` checked as a criterion: named, its query one kiln's monitors read with its comparison and bound after it, kept as written. */
function criterionOf(ask: Record<string, unknown>): Omit<Criterion, "id"> | No {
  const name = line(ask.name, "a criterion's name");
  if (isNo(name)) return name;
  if (!name) return no(400, "a criterion needs its name");
  const query = String(ask.query ?? "").trim();
  if (!query) return no(400, "a criterion's `query` is what it measures, in kiln's monitoring syntax");
  const [op, bound] = [ask.op as Op, ask.bound as number];
  if (!Number.isFinite(bound)) return no(400, "a criterion's `bound` is a number");
  const whole = `${query} ${op} ${bound}`;
  const read = checkMonitor(whole);
  if (read.ok !== true) return no(400, `kiln's monitors can't read \`${whole}\`: ${String(read.error)}`);
  // Its own comparison in its query: what's compared is the query alone.
  if (read.op !== op || read.threshold !== bound || /(>=|<=|==|!=|>|<)\s*-?\d+(?:\.\d+)?\s*$/.test(query))
    return no(400, `a criterion's \`query\` is what's compared, its comparison apart (\`op\`, \`bound\`): not ${query}`);
  return { name, query, op, bound };
}

/** Variant `v` made definition `d`'s next version (experiments' `promotion`): its engine, a microVM's Agentfile where it's another, its size, its own model. */
function promoteInto(d: AgentDef, v: Variant): void {
  const next = settingsOf(d);
  if (v.engine === "host") {
    next.engine = "host";
    next.agentfile = null;
    delete next.cpus;
    delete next.memory;
  } else {
    delete next.engine;
    const inUse = d.engine === "host" ? null : (d.agentfile ?? `harness-${d.harness}`);
    if (v.agentfile !== inUse) next.agentfile = v.agentfile;
    next.cpus = v.cpus;
    next.memory = v.memory;
    if (next.cpus === undefined) delete next.cpus;
    if (next.memory === undefined) delete next.memory;
  }
  if (v.model) next.model = v.model;
  if (same(next, d)) return;
  delete d.engine;
  delete d.cpus;
  delete d.memory;
  Object.assign(d, next, { version: d.version + 1, updated_at: now() });
  versions.get(d.id)?.unshift({ version: d.version, at: d.updated_at, by: BY, ...next });
}

/** The operation a request under `/api/agentdefs/{id}/experiments` is (`rest`, its path after that), and what its path names (ops.rs `op_of`). */
function opOf(m: string, rest: string[]): [string, Record<string, string>] | null {
  const [e, what, sub, ...more] = rest;
  if (more.length) return null;
  if (e === undefined) return m === "POST" ? ["create", {}] : null;
  if (what === undefined) return m === "PATCH" ? ["rename", { experiment: e }] : m === "DELETE" ? ["delete", { experiment: e }] : null;
  if (sub === undefined) {
    if (m !== "POST") return null;
    const op = { weights: "weights", pause: "pause", resume: "resume", stop: "stop", promote: "promote", variants: "variants.add", criteria: "criteria.add" }[what];
    return op ? [op, { experiment: e }] : null;
  }
  if (what === "variants" && m === "PATCH") return ["rename", { experiment: e, variant: sub }];
  if (what === "variants" && m === "DELETE") return ["variants.remove", { experiment: e, variant: sub }];
  if (what === "criteria" && m === "DELETE") return ["criteria.remove", { experiment: e, criterion: sub }];
  return null;
}

/** One of the experiments' operations on definition `def` (P7.1): its answer, or its refusal. */
function experimentOp(op: string, def: string, input: Record<string, unknown>): unknown {
  // Its input read as serde reads it, a new experiment's variants and criteria each.
  const bad = read(input, INPUTS[op] ?? ROUTE, "an operation's input");
  if (bad) return bad;
  if (op === "create") {
    const each = [...(input.variants as unknown[]).map((v) => read(v, VARIANT_INPUT, "a variant")), ...((input.criteria ?? []) as unknown[]).map((c) => read(c, CRITERION_INPUT, "a criterion"))];
    const wrong = each.find((x) => x !== null);
    if (wrong) return wrong;
  }
  // Whose: kiln's own take none; another's is as no definition at all (the mock has one person: any it doesn't keep).
  if (def.startsWith("kiln:")) return no(403, `${def} is kiln's own agent definition, and takes no experiment: copy it as yours (agentdefs.copy) and experiment on that`);
  const d = kept.get(def);
  if (!d) return no(404, "no such agent definition");
  const answer = (e: Experiment) => ({ experiment: experimentOf(e) });

  if (op === "create") {
    const name = line(input.name, "an experiment's name");
    if (isNo(name)) return name;
    if (!name) return no(400, "an experiment needs its name");
    const asks = input.variants as Record<string, unknown>[];
    if (asks.length < 2 || asks.length > 100) return no(400, "an experiment splits its agent's new sessions between 2 and 100 variants (`variants`): a split has 100 parts");
    const split = whole(asks.map((v) => v.weight as number));
    if (split) return split;
    const made: Variant[] = [];
    for (const [k, v] of asks.entries()) {
      const dep = deployment(d, v, `variant ${k + 1}`);
      if (isNo(dep)) return dep;
      made.push({ ...dep, id: randomBytes(16).toString("hex"), weight: v.weight as number, sessions: 0 });
    }
    const criteria: Criterion[] = [];
    for (const c of (input.criteria ?? []) as Record<string, unknown>[]) {
      const k = criterionOf(c);
      if (isNo(k)) return k;
      criteria.push({ ...k, id: randomBytes(16).toString("hex") });
    }
    const live = liveBeside(d.id, "");
    if (live) return oneLive(live.name);
    const at = Date.now();
    const e: Experiment = { id: randomBytes(16).toString("hex"), agentdef: d.id, name, state: "live", state_at: at, version: 1, started_at: at, by: BY, criteria, variants: made, dropped: 0 };
    experiments().push(e);
    return answer(e);
  }
  // Checked before its experiment is looked for, as the server checks them.
  let ready: Omit<Variant, "id" | "weight" | "sessions"> | Omit<Criterion, "id"> | null = null;
  if (op === "rename") {
    const named = line(input.name ?? "", input.variant ? "a variant's name" : "an experiment's name");
    if (isNo(named)) return named;
    if (!input.variant && !named) return no(400, "an experiment needs its name");
  }
  if (op === "variants.add") {
    const dep = deployment(d, input, "the new variant");
    if (isNo(dep)) return dep;
    ready = dep;
  }
  if (op === "criteria.add") {
    const k = criterionOf(input);
    if (isNo(k)) return k;
    ready = k;
  }
  const e = experiments().find((x) => x.id === input.experiment && x.agentdef === d.id);
  if (!e) return no(404, "no such experiment");
  const noVariant = (v: unknown) => no(404, `${e.name} has no variant ${String(v)}`);
  switch (op) {
    case "rename": {
      const named = line(input.name ?? "", input.variant ? "a variant's name" : "an experiment's name") as string | null;
      if (input.variant) {
        const v = e.variants.find((x) => x.id === input.variant);
        if (!v) return noVariant(input.variant);
        v.name = named;
      } else e.name = named ?? e.name;
      return answer(e);
    }
    case "weights": {
      if (!running(e)) return over(e);
      const base = input.base as number | undefined | null;
      if (base !== undefined && base !== null && base !== e.version)
        return no(409, `its split was changed since version ${base}, which this change was made over: it's version ${e.version}. Read it again and make your change over that (\`base\` ${e.version})`);
      const weights = input.weights as Record<string, number>;
      const stray = Object.keys(weights)
        .sort()
        .find((k) => !e.variants.some((v) => v.id === k));
      if (stray !== undefined) return noVariant(stray);
      const unweighted = e.variants.filter((v) => !(v.id in weights)).map(shown);
      if (unweighted.length) return no(400, `say every variant's weight: ${unweighted.join(", ")} has none`);
      const next = e.variants.map((v) => weights[v.id] ?? 0);
      const split = whole(next);
      if (split) return split;
      if (e.variants.some((v, k) => v.weight !== next[k])) {
        e.variants.forEach((v, k) => (v.weight = next[k] ?? v.weight));
        e.version += 1;
      }
      return answer(e);
    }
    case "variants.add": {
      if (!running(e)) return over(e);
      if (e.variants.length >= 100) return no(409, `${e.name} has 100 variants, as many as a split of 100 parts can give one each`);
      e.variants.push({ ...(ready as Omit<Variant, "id" | "weight" | "sessions">), id: randomBytes(16).toString("hex"), weight: 0, sessions: 0 });
      return answer(e);
    }
    case "variants.remove": {
      if (!running(e)) return over(e);
      const gone = e.variants.find((v) => v.id === input.variant);
      if (!gone) return noVariant(input.variant);
      if ((e.promoted ?? e.variants[0]?.id) === gone.id) return no(409, `${shown(gone)} is what its agent runs now: the split keeps it`);
      if (e.variants.length <= 2) return no(409, `${e.name} splits between 2 variants, the fewest a split has: stop it instead`);
      const rest = e.variants.filter((v) => v !== gone);
      if (gone.weight > 0) {
        const next = rescale(rest.map((v) => v.weight));
        if (!next) return no(409, `${shown(gone)} takes every new session: give the others their share first (agentdefs.experiments.weights)`);
        rest.forEach((v, k) => (v.weight = next[k] ?? v.weight));
        e.version += 1;
      }
      e.dropped += gone.sessions;
      e.variants = rest;
      return answer(e);
    }
    case "criteria.add":
      e.criteria.push({ ...(ready as Omit<Criterion, "id">), id: randomBytes(16).toString("hex") });
      return answer(e);
    case "criteria.remove": {
      const at = e.criteria.findIndex((c) => c.id === input.criterion);
      if (at < 0) return no(404, `${e.name} has no criterion ${String(input.criterion)}`);
      e.criteria.splice(at, 1);
      return answer(e);
    }
    case "pause":
    case "resume":
    case "stop": {
      const asIs = (op === "pause" && e.state === "paused") || (op === "resume" && e.state === "live") || (op === "stop" && e.state === "stopped");
      if (asIs) return answer(e);
      if (!running(e)) return over(e);
      if (op === "resume") {
        const live = liveBeside(d.id, e.id);
        if (live) return oneLive(live.name);
      }
      e.state = op === "pause" ? "paused" : op === "resume" ? "live" : "stopped";
      e.state_at = Date.now();
      return answer(e);
    }
    case "promote": {
      const v = e.variants.find((x) => x.id === input.variant);
      if (!v) return noVariant(input.variant);
      if (e.state === "promoted" && e.promoted === v.id) return { experiment: experimentOf(e), agentdef: d };
      if (!running(e)) return over(e);
      // Its deployment the definition's next version; the split's weights as they were.
      promoteInto(d, v);
      e.state = "promoted";
      e.promoted = v.id;
      e.state_at = Date.now();
      return { experiment: experimentOf(e), agentdef: d };
    }
    case "delete":
      keptExperiments = experiments().filter((x) => x !== e);
      return { deleted: true };
  }
  return no(404, `no operation ${op}`);
}

/* ---------------------------------------- the routes */

/** Handles the routes above; false when it doesn't know the route. */
export async function handleAgentdefs(req: http.IncomingMessage, res: http.ServerResponse, url: URL, ctx: Ctx): Promise<boolean> {
  const p = url.pathname;
  const m = req.method ?? "GET";
  if (p === "/api/agentdef-sessions" && m === "GET")
    return (seed(), ctx.json(res, 200, Object.fromEntries(Object.entries(SESSIONS).map(([id, d]) => [id, { sessions: d.sessions, starts: served(d.starts, d.sessions.length) }]))), true);
  if (p === "/api/agentdef-experiments" && m === "GET") {
    seed();
    // One definition's (kiln's own have none; another's is as none), else all the person's: the mock's one person reaches every one.
    const id = url.searchParams.get("id")?.trim();
    if (!id)
      return (
        ctx.json(
          res,
          200,
          listed(() => true),
        ),
        true
      );
    if (id.startsWith("kiln:")) return (ctx.json(res, 200, {}), true);
    if (!kept.has(id)) return (ctx.json(res, 404, { error: "no such agent definition" }), true);
    return (
      ctx.json(
        res,
        200,
        listed((e) => e.agentdef === id),
      ),
      true
    );
  }
  if (p === "/api/mock/agentdef-experiments" && m === "POST") {
    keptExperiments = seededExperiments();
    return (
      ctx.json(
        res,
        200,
        listed(() => true),
      ),
      true
    );
  }
  if (p !== "/api/agentdefs" && !p.startsWith("/api/agentdefs/")) return false;
  seed();
  const fail = (status: number, error: string) => (ctx.json(res, status, { error }), true);
  const answer = (body: unknown) => (ctx.json(res, 200, body), true);
  // A definition's experiments, kiln's own's (refused) among them: each route its operation, its path naming what it takes.
  const segs = p.split("/").slice(3).map(decodeURIComponent);
  if (segs[1] === "experiments" && segs[0]) {
    const route = opOf(m, segs.slice(2));
    if (!route) return fail(404, `no route ${m} ${p.slice(1)}`);
    let b: unknown;
    try {
      b = await ctx.body<unknown>(req);
    } catch (e) {
      return fail(400, `the body isn't JSON: ${String(e)}`);
    }
    if (!isObject(b)) return fail(400, "the body is a JSON object");
    const out = experimentOp(route[0], segs[0], { ...b, id: segs[0], ...route[1] });
    return isNo(out) ? fail(out.status, out.error) : answer(out);
  }
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
    // Its experiments go with it (experiments' `forget_in`).
    keptExperiments = experiments().filter((e) => e.agentdef !== d.id);
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
    // Where it runs and its size are that version's too (a promotion's, or none).
    delete d.engine;
    delete d.cpus;
    delete d.memory;
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
