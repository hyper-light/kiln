/**
 * Ship's Pipelines in the mock (docs/ci.md W29; Ada's approved stills of 2026-10-31, pl-1 to pl-9):
 * three repositories' workflows and actions as the server answers `GET /api/ship/pipelines` (each
 * row's stand, word and pips as `kiln_ci::pipelines` says them, in its order: what needs someone
 * first, then drafts, then by name; repositories needing someone first, then by name), each one's
 * pane (`GET /api/ship/pipelines/:owner/:name?path=`), and the strip's verbs (`POST
 * /api/ship/pipelines`). What each browser sent is kept by its user agent, as Deploys' is, so the e2e
 * projects running side by side don't see each other's.
 */
import type http from "node:http";
import { parse, parseDocument } from "yaml";
import type { ShipPipelineDraft, ShipPipelineGroup, ShipPipelineLine, ShipPipelinePane, ShipPipelineRepo, ShipPipelineRow, ShipPipelineVerb, ShipPipelines } from "../src/api/ship.ts";
import { FILES, RUNS, TAGS, UPDATES, USED_BY } from "./pipelines-files.ts";

type Ctx = { json: (res: http.ServerResponse, status: number, body: unknown) => void };

/** A request's JSON body; what doesn't read as JSON is an empty one. */
async function readJson(req: http.IncomingMessage): Promise<Record<string, unknown>> {
  const chunks: Buffer[] = [];
  for await (const c of req) chunks.push(c as Buffer);
  try {
    return JSON.parse(Buffer.concat(chunks).toString("utf8") || "{}") as Record<string, unknown>;
  } catch {
    return {};
  }
}

const NOW = Math.floor(Date.now() / 1000);
const ago = (s: number) => NOW - s;
const M = 60;
const H = 3600;
const D = 86400;

/** Each repository's default branch and its head. */
const HEADS: Record<string, { ref: string; commit: string }> = {
  "hyper-light/kiln": { ref: "main", commit: "a3f2c1d9e8b7a6f5e4d3c2b1a09f8e7d6c5b4a39" },
  "hyper-light/ci-kit": { ref: "main", commit: "e41a9c2b7d6e5f4a3b2c1d0e9f8a7b6c5d4e3f21" },
  "hyper-light/quiver": { ref: "main", commit: "1f9b2c4a8e7d6c5b4a3f2e1d0c9b8a7f6e5d4c38" },
};

type Seed = Omit<ShipPipelineRow, "key" | "ref" | "commit" | "v" | "released" | "draft"> & { released?: string; draft?: ShipPipelineDraft };

const draft = (id: string, state: ShipPipelineDraft["state"], author: string, change: string | null): ShipPipelineDraft => ({
  id,
  state,
  author,
  change: change ? { kind: "change", name: change, url: null } : null,
});

/** The stills' rows, by repository, before the server's order. */
const SEEDS: Record<string, { access: string; rows: Seed[] }> = {
  "hyper-light/kiln": {
    access: "none",
    rows: [
      {
        kind: "workflow",
        dialect: "kiln",
        name: "stress",
        path: ".kiln/stress.yaml",
        stand: "needs_you",
        word: "Review",
        line: "agent:01a19350 for ada · 2 changes · plans 114 jobs (was 113)",
        pips: ["passed", "passed", "passed"],
        who: "agent:01a19350c2d4",
        when: ago(4 * M),
        draft: draft("d-stress", "review", "agent:01a19350c2d4", "#214"),
      },
      {
        kind: "workflow",
        dialect: "kiln",
        name: "e2e",
        path: ".kiln/e2e.yaml",
        stand: "failing",
        word: "Failing",
        line: "failed on 9e1b2c0 · 3 of 62 jobs: ship-deploys, ship-lanes, ship-sweep",
        pips: ["passed", "passed", "failed"],
        who: "ada",
        when: ago(2 * H),
      },
      {
        kind: "workflow",
        dialect: "kiln",
        name: "ci",
        path: ".kiln/ci.yaml",
        stand: "shipped",
        word: "Passing",
        line: "push to main, pull requests · 5 jobs on bare metal · passed on a3f2c1d",
        pips: ["passed", "passed", "passed"],
        who: "ada",
        when: ago(12 * M),
      },
      {
        kind: "workflow",
        dialect: "kiln",
        name: "release",
        path: ".kiln/release.yaml",
        stand: "shipped",
        word: "Passing",
        line: "tags v* · linux, windows and macOS on bare metal · passed on v0.2.0",
        pips: ["passed", "passed", "passed"],
        who: "admin",
        when: ago(D),
      },
      {
        kind: "workflow",
        dialect: "kiln",
        name: "nightly",
        path: ".kiln/nightly.ts",
        stand: "shipped",
        word: "Passing",
        line: "TypeScript · 03:00 UTC and by hand · 2 jobs, 4 legs · passed 7h ago",
        pips: ["passed", "passed", "passed"],
        who: "ada",
        when: ago(7 * H),
      },
      {
        kind: "workflow",
        dialect: "kiln",
        name: "smoke",
        path: ".kiln/smoke.yaml",
        stand: "shipped",
        word: "Passing",
        line: "after each deploy, nightly and by hand · 2 jobs · passed after #153's sync",
        pips: ["passed", "passed", "passed"],
        who: "ada",
        when: ago(6 * H),
      },
      {
        kind: "workflow",
        dialect: "gha",
        name: "GitHub CI",
        path: ".github/workflows/ci.yml",
        stand: "superseded",
        word: "Not run here",
        line: ".github/workflows/ci.yml · this repository's .kiln/ workflows run instead",
        pips: ["passed", "none", "none"],
        who: "ada",
        when: ago(9 * D),
      },
    ],
  },
  "hyper-light/ci-kit": {
    access: "owner",
    rows: [
      {
        kind: "shards",
        dialect: null,
        name: "sandbox",
        path: ".github/actions/sandbox",
        stand: "needs_you",
        word: "Can't run",
        line: "Shards action · no machine runs shards microVMs yet",
        pips: ["passed", "blocked", "none"],
        who: "ada",
        when: ago(7 * D),
        released: "v0.1.0",
      },
      {
        kind: "firecracker",
        dialect: null,
        name: "fuzz-target",
        path: ".github/actions/fuzz-target",
        stand: "waiting",
        word: "Unpublished",
        line: "Firecracker action · no trigger runs it and no repository can use it yet",
        pips: ["passed", "passed", "none"],
        who: "ada",
        when: ago(2 * M),
        draft: draft("d-fuzz", "unpublished", "ada", "#17"),
      },
      {
        kind: "composite",
        dialect: null,
        name: "setup-rust",
        path: ".github/actions/setup-rust",
        stand: "shipped",
        word: "Released",
        line: "composite action · used by 6 repositories · v2.4.0 3 days ago",
        pips: ["passed", "passed", "passed"],
        who: "ada",
        when: ago(3 * D),
        released: "v2.4.0",
      },
      {
        kind: "composite",
        dialect: null,
        name: "cache-cargo",
        path: ".github/actions/cache-cargo",
        stand: "shipped",
        word: "Released",
        line: "composite action · used by 4 repositories · v2.4.0 3 days ago",
        pips: ["passed", "passed", "passed"],
        who: "ada",
        when: ago(3 * D),
        released: "v2.4.0",
      },
      {
        kind: "javascript",
        dialect: null,
        name: "notify",
        path: ".github/actions/notify",
        stand: "shipped",
        word: "Released",
        line: "JavaScript action · node24 · used by 3 repositories · v2.4.0 3 days ago",
        pips: ["passed", "passed", "passed"],
        who: "ada",
        when: ago(5 * D),
        released: "v2.4.0",
      },
      {
        kind: "docker",
        dialect: null,
        name: "lint-docker",
        path: ".github/actions/lint-docker",
        stand: "shipped",
        word: "Released",
        line: "Docker action · a container per call · used by 2 repositories · v2.4.0",
        pips: ["passed", "passed", "passed"],
        who: "ada",
        when: ago(12 * D),
        released: "v2.4.0",
      },
      {
        kind: "reusable",
        dialect: "gha",
        name: "rust-check",
        path: ".github/workflows/rust-check.yml",
        stand: "shipped",
        word: "Released",
        line: "reusable workflow · clippy and tests · used by 5 repositories · v2.4.0",
        pips: ["passed", "passed", "passed"],
        who: "ada",
        when: ago(3 * D),
        released: "v2.4.0",
      },
    ],
  },
  "hyper-light/quiver": {
    access: "none",
    rows: [
      {
        kind: "workflow",
        dialect: "gha",
        name: "ci",
        path: ".github/workflows/ci.yml",
        stand: "waiting",
        word: "Update",
        line: "ci-kit's rust-check v2.3.0 → v2.4.0 · plans the same 2 jobs",
        pips: ["passed", "passed", "running"],
        who: "kiln-updates:ci-kit",
        when: ago(3 * D),
        draft: draft("d-update-88", "update", "kiln-updates:ci-kit", "#88"),
      },
      {
        kind: "workflow",
        dialect: "gha",
        name: "ci",
        path: ".github/workflows/ci.yml",
        stand: "shipped",
        word: "Passing",
        line: "push to main, pull requests · 2 jobs, 4 legs · passed on 1f9b2c4",
        pips: ["passed", "passed", "passed"],
        who: "ada",
        when: ago(3 * D),
      },
      {
        kind: "workflow",
        dialect: "gha",
        name: "bench",
        path: ".github/workflows/bench.yml",
        stand: "shipped",
        word: "Passing",
        line: "nightly · bare metal, labelled bench · passed 4d ago",
        pips: ["passed", "passed", "passed"],
        who: "ada",
        when: ago(4 * D),
      },
    ],
  },
};

/** W29-R02's order: Review, Failing, Can't run; then drafts; then the rest by name, then path. */
function rank(r: Seed): [number, number] {
  const need = r.word === "Review" ? 0 : r.word === "Failing" ? 1 : r.word === "Can't run" ? 2 : 3;
  const draftRank = r.draft && r.draft.state !== "review" ? 1 : 2;
  return [need < 3 ? 0 : draftRank, need];
}

function rowOf(repo: string, s: Seed): ShipPipelineRow {
  const head = HEADS[repo] ?? { ref: "main", commit: "0".repeat(40) };
  const key = s.draft ? `${repo}:${s.path}:draft:${s.draft.id}` : `${repo}:${s.path}`;
  const row: Omit<ShipPipelineRow, "v"> = { ...s, key, ref: head.ref, commit: head.commit, released: s.released ?? null, draft: s.draft ?? null };
  return { ...row, v: JSON.stringify(row).length.toString(16) };
}

/** A repository's heading words, as `kiln_ci::pipelines::heading` makes them. */
function heading(rows: readonly Seed[], shared: boolean): { say: string; needs: number } {
  const action = (r: Seed) => !["workflow", "reusable"].includes(r.kind);
  const workflows = rows.filter((r) => !action(r)).length;
  const actions = rows.filter(action).length;
  const drafts = rows.filter((r) => r.draft && (r.draft.state === "unpublished" || r.draft.state === "not_saved")).length;
  const updates = [...new Set(rows.filter((r) => r.draft?.state === "update").map(() => "ci-kit v2.4.0"))];
  const failing = rows.filter((r) => r.stand === "failing").length;
  const waits = rows.filter((r) => r.draft?.state === "review").length;
  const needs = rows.filter((r) => rank(r)[0] === 0).length;
  const n = (k: number, one: string, many: string) => `${k} ${k === 1 ? one : many}`;
  const kinds: [number, string, string][] = [
    [workflows, "workflow", "workflows"],
    [actions, "action", "actions"],
  ];
  if (actions > workflows) kinds.reverse();
  const parts = [
    ...(shared ? ["shared"] : []),
    ...kinds.filter(([k]) => k > 0).map(([k, one, many]) => n(k, one, many)),
    ...(drafts ? [n(drafts, "draft", "drafts")] : []),
    ...updates.map((u) => `${u} is out`),
    ...(failing ? [`${failing} failing`] : []),
    ...(waits ? [n(waits, "waits on you", "wait on you")] : []),
  ];
  return { say: parts.join(" · "), needs };
}

function repoOf(name: string): ShipPipelineRepo {
  const seed = SEEDS[name] ?? { access: "none", rows: [] };
  const sorted = [...seed.rows].sort((a, b) => {
    const [ra, na] = rank(a);
    const [rb, nb] = rank(b);
    return ra - rb || na - nb || a.name.toLowerCase().localeCompare(b.name.toLowerCase()) || a.path.localeCompare(b.path) || (a.draft ? 0 : 1) - (b.draft ? 0 : 1);
  });
  const { say, needs } = heading(seed.rows, seed.access !== "none");
  const head = HEADS[name] ?? { ref: "main", commit: "0".repeat(40) };
  return { repo: name, say, needs, access: seed.access, ref: head.ref, commit: head.commit, rows: sorted.map((s) => rowOf(name, s)) };
}

function list(q: string): ShipPipelines {
  const words = q.trim().toLowerCase();
  const repos = Object.keys(SEEDS)
    .map(repoOf)
    .map((r) => (words && !r.repo.toLowerCase().includes(words) ? { ...r, rows: r.rows.filter((x) => [x.name, x.path, x.kind, x.word].some((v) => v.toLowerCase().includes(words))) } : r))
    .filter((r) => r.rows.length > 0 || !words);
  repos.sort((a, b) => Number(b.needs > 0) - Number(a.needs > 0) || a.repo.localeCompare(b.repo));
  return { repos };
}

/** An event's words, its group in Add a trigger, and whose it is, as `kiln_ci::pipelines` says them (W29-R06). */
const EVENTS: Record<string, [string, string, "both" | "github" | "kiln"]> = {
  push: ["push", "Code", "both"],
  pull_request: ["pull request", "Code", "both"],
  diff: ["diff", "Code", "kiln"],
  land: ["land", "Code", "kiln"],
  merge_group: ["merge group", "Code", "both"],
  create: ["create", "Code", "both"],
  delete: ["delete", "Code", "both"],
  schedule: ["schedule", "Time", "both"],
  workflow_dispatch: ["manual", "People and agents", "both"],
  agent: ["agent", "People and agents", "kiln"],
  comment: ["comment", "People and agents", "kiln"],
  review: ["review", "People and agents", "kiln"],
  workflow_run: ["pipeline", "Other workflows", "both"],
  pipeline: ["pipeline", "Other workflows", "kiln"],
  workflow_call: ["called", "Other workflows", "both"],
  deploy: ["deploy", "Deploys and releases", "kiln"],
  release: ["release", "Deploys and releases", "both"],
  registry_package: ["package", "Deploys and releases", "both"],
  webhook: ["webhook", "Outside kiln", "kiln"],
  repository_dispatch: ["dispatch", "Outside kiln", "both"],
};

type Obj = Record<string, unknown>;
const obj = (v: unknown): Obj => (v && typeof v === "object" && !Array.isArray(v) ? (v as Obj) : {});
/** A file's YAML as an object; what doesn't read is an empty one. */
function yamlOf(text: string): Obj {
  try {
    return obj(parse(text));
  } catch {
    return {};
  }
}
const strs = (v: unknown): string[] => (Array.isArray(v) ? v.map(String) : v === undefined || v === null ? [] : [String(v)]);

/** A trigger's filters in a line, in the server's order: types, branches, tags, paths, schedules, workflows, inputs, the rest's names. */
function filters(event: string, v: unknown): string {
  const o = obj(v);
  const parts: string[] = [];
  if (o.types) parts.push(strs(o.types).join(", "));
  for (const [k, w] of [
    ["branches", "branches"],
    ["branches-ignore", "not branches"],
    ["tags", "tags"],
    ["tags-ignore", "not tags"],
    ["paths", "paths"],
    ["paths-ignore", "not paths"],
  ] as const)
    if (strs(o[k]).length) parts.push(`${w} ${strs(o[k]).join(", ")}`);
  if (event === "schedule") for (const s of Array.isArray(v) ? v : []) parts.push(`${String(obj(s).cron)} ${String(obj(s).timezone ?? "UTC")}`);
  if (o.workflows || (event === "pipeline" && o.name)) parts.push(`after ${strs(o.workflows ?? o.name).join(", ")}`);
  if (o.inputs && event !== "schedule") parts.push(`inputs: ${Object.keys(obj(o.inputs)).join(", ")}`);
  const known = new Set(["types", "branches", "branches-ignore", "tags", "tags-ignore", "paths", "paths-ignore", "workflows", "inputs", "drafts", ...(event === "pipeline" ? ["name"] : [])]);
  for (const k of Object.keys(o)) if (!known.has(k)) parts.push(k);
  return parts.join(" · ");
}

function triggerLines(on: unknown, kiln: boolean): ShipPipelineLine[] {
  const entries: [string, unknown][] = typeof on === "string" ? [[on, null]] : Array.isArray(on) ? on.map((e): [string, unknown] => [String(e), null]) : Object.entries(obj(on));
  const out: ShipPipelineLine[] = entries
    .filter(([e, v]) => !(kiln && e === "manual" && v === false))
    .map(([e, v]) => {
      const event = kiln && e === "manual" ? "workflow_dispatch" : e;
      const [words, group, whose] = EVENTS[event] ?? [event.replace(/_/g, " "), "Repository", "github"];
      return { key: `on.${event}`, kind: "trigger", name: words, value: filters(event, v), group, whose };
    });
  // kiln's dialect runs by hand unless `manual: false` says it doesn't (W02-R06).
  if (kiln && !out.some((l) => l.key === "on.workflow_dispatch") && obj(on).manual !== false)
    out.push({ key: "on.workflow_dispatch", kind: "trigger", name: "manual", value: "", group: "People and agents", whose: "both" });
  return out;
}

/** Jobs by how far down their needs go (W29-R04's Stage 1, 2…). */
function stagesOf(jobs: Obj): string[][] {
  const depth = new Map<string, number>();
  const needsOf = (id: string) => strs(obj(jobs[id]).needs);
  for (let pass = 0; pass < 32; pass++)
    for (const id of Object.keys(jobs)) {
      if (depth.has(id)) continue;
      const n = needsOf(id);
      if (n.every((x) => depth.has(x))) depth.set(id, n.length ? Math.max(...n.map((x) => depth.get(x) ?? 0)) + 1 : 0);
    }
  const out: string[][] = [];
  for (const [id, d] of depth) (out[d] ??= []).push(id);
  return out.filter(Boolean);
}

const fromWords = (u: string) =>
  u.startsWith("./")
    ? "this repository"
    : u.startsWith("docker://")
      ? "a container image"
      : u.startsWith("kiln://")
        ? "a repository kiln hosts"
        : u.startsWith("kiln/")
          ? "kiln's own"
          : "fetched by name";

function workflowGroups(key: string, text: string, kiln: boolean, runsHere: boolean): ShipPipelineGroup[] {
  const doc = yamlOf(text);
  const jobs = obj(doc.jobs);
  const out: ShipPipelineGroup[] = [{ name: "Triggers", lines: triggerLines(doc.on, kiln) }];
  const call = obj(obj(doc.on).workflow_call);
  if (Object.keys(call).length)
    out.push({
      name: "Called with",
      lines: Object.entries(obj(call.inputs)).map(([n, i]) => ({
        key: `call.inputs.${n}`,
        kind: "input",
        name: n,
        value: `${String(obj(i).type ?? "string")}${obj(i).required ? " · required" : ""}`,
      })),
    });
  stagesOf(jobs).forEach((ids, i) =>
    out.push({
      name: `Stage ${i + 1}`,
      lines: ids.map((id) => {
        const j = obj(jobs[id]);
        const labels = strs(j["runs-on"]).join(", ");
        let value = j.uses ? `calls ${String(j.uses)}` : [labels, j.container ? "in a container" : "bare metal"].filter(Boolean).join(" · ");
        if (strs(j.needs).length) value += ` · needs ${strs(j.needs).join(", ")}`;
        return { key: `jobs.${id}`, kind: "job", name: String(j.name ?? id), value };
      }),
    }),
  );
  const seen = new Set<string>();
  const uses: ShipPipelineLine[] = [];
  for (const j of Object.values(jobs).map(obj)) {
    const all: [string, boolean][] = [
      ...(j.uses ? [[String(j.uses), true] as [string, boolean]] : []),
      ...(Array.isArray(j.steps)
        ? j.steps
            .map(obj)
            .filter((s) => s.uses)
            .map((s): [string, boolean] => [String(s.uses), false])
        : []),
    ];
    for (const [u, isCall] of all) {
      if (seen.has(u)) continue;
      seen.add(u);
      const ref = u.includes("@") ? u.slice(u.lastIndexOf("@") + 1) : "";
      uses.push({ key: `uses.${u}`, kind: "use", name: u, value: `${isCall ? "a reusable workflow, " : ""}${fromWords(u)}${ref ? ` · ${ref}` : ""}` });
    }
  }
  out.push({ name: "Uses", lines: uses });
  const names = (re: RegExp) => [...new Set([...text.matchAll(re)].map((m) => m[1] ?? ""))];
  out.push({
    name: "Secrets and variables",
    lines: [
      ...names(/secrets\.([A-Za-z_][A-Za-z0-9_]*)/g).map((n) => ({ key: `secrets.${n}`, kind: "secret", name: n, value: "used" })),
      ...names(/vars\.([A-Za-z_][A-Za-z0-9_]*)/g).map((n) => ({ key: `vars.${n}`, kind: "variable", name: n, value: "used" })),
    ],
  });
  out.push({
    name: "Runs",
    lines: (RUNS[key] ?? []).map((r) => ({
      key: `runs.${r.n}`,
      kind: "run",
      name: `#${r.n}`,
      value: `${r.event} · ${r.status}`,
      state: r.status === "passed" ? "passed" : r.status === "failed" ? "failed" : "running",
    })),
  });
  const used = USED_BY[key.replace(/:draft:.*$/, "")];
  if (Object.keys(call).length && used) out.push({ name: "Used by", lines: usedByLines(used) });
  out.push({
    name: "Settings",
    lines: [
      runsHere
        ? { key: "settings.on", kind: "setting", name: "Turned on", value: "runs on its triggers" }
        : { key: "settings.on", kind: "setting", name: "Not run here", value: "this repository's .kiln/ workflows run instead" },
      { key: "settings.machines", kind: "setting", name: "Machines", value: "bare metal unless a job says" },
      ...(doc.concurrency ? [{ key: "settings.concurrency", kind: "setting", name: "Concurrency", value: String(doc.concurrency) }] : []),
      ...(Object.keys(call).length ? [{ key: "settings.access", kind: "setting", name: "Access", value: "shared with hyper-light's repositories", state: "owner" }] : []),
    ],
  });
  return out;
}

const usedByLines = (used: { repo: string; ref: string; sha: string; runs: number }[]): ShipPipelineLine[] =>
  used.map((c) => ({ key: `used_by.${c.repo}@${c.ref}`, kind: "consumer", name: c.repo, value: `${c.ref} · ${c.sha} · ${c.runs === 1 ? "1 run" : `${c.runs} runs`}` }));

function actionGroups(repo: string, key: string, text: string, kind: string, head: string): ShipPipelineGroup[] {
  const doc = yamlOf(text);
  const runs = obj(doc.runs);
  const out: ShipPipelineGroup[] = [
    {
      name: "Inputs",
      lines: Object.entries(obj(doc.inputs)).map(([n, i]) => {
        const o = obj(i);
        const v = o.required === true ? "required" : o.default !== undefined ? `default ${String(o.default)}` : "optional";
        return { key: `inputs.${n}`, kind: "input", name: n, value: o.description ? `${v} · ${String(o.description)}` : v };
      }),
    },
    { name: "Outputs", lines: Object.entries(obj(doc.outputs)).map(([n, o]) => ({ key: `outputs.${n}`, kind: "output", name: n, value: String(obj(o).description ?? "") })) },
  ];
  if (kind === "composite")
    out.push({
      name: "Steps",
      lines: (Array.isArray(runs.steps) ? runs.steps.map(obj) : []).map((s, i) => ({
        key: `runs.steps.${i}`,
        kind: "step",
        name: String(s.name ?? s.id ?? `step ${i + 1}`),
        value: String(s.uses ?? String(s.run ?? "").split("\n")[0] ?? ""),
      })),
    });
  else if (kind === "javascript")
    out.push({
      name: `Runs on Node (${String(runs.using)})`,
      lines: ["main", "pre", "post"].filter((k) => runs[k]).map((k) => ({ key: `runs.${k}`, kind: "entry", name: k, value: String(runs[k]) })),
    });
  else if (kind === "docker")
    out.push({
      name: "Image",
      lines: [
        { key: "runs.image", kind: "image", name: "image", value: String(runs.image ?? "") },
        ...(runs.args ? [{ key: "runs.args", kind: "entry", name: "args", value: strs(runs.args).join(" ") }] : []),
      ],
    });
  else
    out.push({
      name: kind === "firecracker" ? "Firecracker microVM" : "Shards microVM",
      lines: [
        { key: "runs.image", kind: "image", name: "image", value: String(runs.image ?? "") },
        {
          key: "runs.resources",
          kind: "setting",
          name: "resources",
          value:
            Object.entries(obj(runs.resources))
              .map(([k, v]) => `${k} ${String(v)}`)
              .join(", ") || "as the leg's",
        },
        { key: "runs.egress", kind: "setting", name: "egress", value: strs(runs.egress).join(", ") || "none" },
      ],
    });
  out.push({
    name: "Versions",
    lines: [
      { key: "versions.head", kind: "version", name: "the default branch's tip", value: head.slice(0, 12) },
      ...(TAGS[repo] ?? []).map(([t, sha]) => ({ key: `versions.${t}`, kind: "version", name: t, value: sha.slice(0, 12) })),
    ],
  });
  const used = USED_BY[key];
  out.push({ name: "Used by", lines: used ? usedByLines(used) : [{ key: "used_by", kind: "note", name: "Who uses it", value: "no other repository's runs have used it yet", state: "none" }] });
  out.push({ name: "Settings", lines: [{ key: "settings.access", kind: "setting", name: "Access", value: "shared with hyper-light's repositories", state: "owner" }] });
  return out;
}

/** The strip's verbs as `kiln_ci::pipelines::verbs_for` gives them, for the mock's viewer, ada (who may submit and land). */
function verbsOf(row: ShipPipelineRow, byHand: boolean): ShipPipelineVerb[] {
  const on = (verb: string, label: string): ShipPipelineVerb => ({ verb, label, enabled: true });
  const dim = (verb: string, label: string, why: string): ShipPipelineVerb => ({ verb, label, enabled: false, why_not: why });
  const d = row.draft;
  const mine = d?.author === "ada";
  const update = d?.state === "update";
  const action = !["workflow", "reusable"].includes(row.kind);
  const run = d
    ? mine
      ? on("run", "Run")
      : dim("run", "Run", "A draft runs only by its author's hand (W29-R14)")
    : row.kind === "reusable"
      ? dim("run", "Run", "A reusable workflow runs where it's called (W29-R14)")
      : row.kind === "workflow" && !byHand
        ? dim("run", "Run", "It has no workflow_dispatch or manual trigger to run it by hand (W29-R14)")
        : on("run", "Run");
  const ready = !d
    ? dim("ready", "Ready", "Ready is for a draft (W29-R12)")
    : update
      ? dim("ready", "Ready", "kiln's update drafts are reviewed as their change (W29-R22)")
      : !mine
        ? dim("ready", "Ready", "Only its author marks it ready (W29-R12)")
        : d.state === "review"
          ? dim("ready", "Ready", "It's in review already (W29-R12)")
          : on("ready", "Ready");
  const discard = !d
    ? dim("discard", "Discard", "Discard is for a draft (W29-R13)")
    : update
      ? dim("discard", "Discard", "kiln's update drafts are reviewed as their change (W29-R22)")
      : !mine
        ? dim("discard", "Discard", "Only its author discards it (W29-R13)")
        : on("discard", "Discard");
  const out = [run, on("save_draft", "Save draft"), ready, discard];
  if (action || row.kind === "reusable")
    out.push(
      d
        ? dim("release", "Release", "A draft is released once it's published (W29-R21)")
        : { ...on("release", "Release"), asks: "Keep · Release: for the workflows that use it, input `targets` added, optional" },
    );
  return out;
}

const KIND_WORDS: Record<string, string> = {
  workflow: "workflow",
  reusable: "reusable workflow",
  composite: "composite action",
  javascript: "JavaScript action",
  docker: "Docker action",
  firecracker: "Firecracker action",
  shards: "Shards action",
};

/** A row's pane: its head, the strip, its groups as the server makes them, and its file. */
function paneOf(repo: string, row: ShipPipelineRow): ShipPipelinePane {
  const key = row.draft ? `${repo}:${row.path}:draft:${row.draft.id}` : `${repo}:${row.path}`;
  const text = FILES[key] ?? "";
  const action = !["workflow", "reusable"].includes(row.kind);
  const kiln = row.dialect === "kiln";
  const groups = action ? actionGroups(repo, key, text, row.kind, row.commit) : workflowGroups(key, text, kiln, row.stand !== "superseded");
  const update = UPDATES[key];
  if (update)
    groups.unshift(
      { name: "What changes for it", lines: update.changes.map(([n, v]) => ({ key: `changes.${n}`, kind: "change", name: n, value: v })) },
      { name: "Upstream", lines: update.upstream.map(([n, v]) => ({ key: `upstream.${n}`, kind: "upstream", name: n, value: v })) },
    );
  const byHand = groups.some((g) => g.name === "Triggers" && g.lines.some((l) => l.key === "on.workflow_dispatch"));
  return {
    head: { title: row.name, path: row.path, kind: row.kind, dialect: row.dialect, ref: row.ref, commit: row.commit, draft: row.draft, stand: row.stand, word: row.word, pips: row.pips },
    strip: { said: [`${KIND_WORDS[row.kind] ?? row.kind} · ${row.line}`], pick: ["all"], verbs: verbsOf(row, byHand) },
    groups,
    detail: { name: row.path, views: action ? ["Changes", "YAML", "Try it"] : ["YAML", "Flow", "Plan"], text },
    repo,
  };
}

/** The labels the mock's machines carry, for the Flow view's and Try it's pickers. */
const LABELS = ["baremetal", "linux", "docker", "bench", "macos", "arm64", "x86_64"].map((label) => ({ label, from: ["runner" as const] }));

/** A minute's wall clock in `tz`, as Intl reads it. */
function wall(ms: number, tz: string): { y: number; mo: number; d: number; h: number; mi: number } {
  const parts = new Intl.DateTimeFormat("en-US", { timeZone: tz, hourCycle: "h23", year: "numeric", month: "2-digit", day: "2-digit", hour: "2-digit", minute: "2-digit" }).formatToParts(new Date(ms));
  const get = (k: string) => Number(parts.find((x) => x.type === k)?.value ?? 0);
  return { y: get("year"), mo: get("month"), d: get("day"), h: get("hour"), mi: get("minute") };
}
/** `tz`'s offset from UTC at `ms`, in ms. */
const offsetAt = (ms: number, tz: string) => {
  const w = wall(ms, tz);
  return Date.UTC(w.y, w.mo - 1, w.d, w.h, w.mi) - Math.floor(ms / 60_000) * 60_000;
};

/**
 * A daily schedule's next three runs (`M H * * *`, what the mock's files say), its local time read in
 * its zone as W03's cron reads it, with the zone's change of offset in the day before each. The
 * server's are `kiln_ci::trigger::cron::next_runs`; this is the mock's stand-in.
 */
function nextDaily(cron: string, tz: string | null): { at: number; shift?: { at: number; local: string; ends: boolean } }[] | null {
  const m = /^(\d+) (\d+) \* \* \*$/.exec(cron.trim());
  if (!m) return null;
  const zone = tz ?? "UTC";
  const [mi, h] = [Number(m[1]), Number(m[2])];
  const today = wall(Date.now(), zone);
  const out: { at: number; shift?: { at: number; local: string; ends: boolean } }[] = [];
  for (let k = 0; k < 5 && out.length < 3; k++) {
    const day = new Date(Date.UTC(today.y, today.mo - 1, today.d + k));
    const guess = Date.UTC(day.getUTCFullYear(), day.getUTCMonth(), day.getUTCDate(), h, mi);
    let t = guess - offsetAt(guess, zone);
    t = guess - offsetAt(t, zone);
    if (t <= Date.now()) continue;
    const [was, now] = [offsetAt(t - 86_400_000, zone), offsetAt(t, zone)];
    out.push({ at: Math.floor(t / 1000), ...(was !== now ? { shift: { at: Math.floor(t / 1000) - 3600, local: "02:00", ends: now < was } } : {}) });
  }
  return out;
}

/** `POST /api/ci/plan-preview` (W04-R29), as the mock plans: a job's legs by its matrix, where linux legs would run. */
function preview(body: Obj): unknown {
  const text = String(body.content ?? FILES[`${String(body.repo)}:${String(body.path)}`] ?? "");
  let doc: Obj;
  try {
    doc = obj(parse(text));
  } catch (e) {
    return { repo: body.repo, ref: "main", sha: "", path: body.path, event: body.event, runs: false, why: `it isn't YAML: ${String(e)}`, diagnostics: [], plan: null };
  }
  const fit = (name: string) => ({ runner: name, name, connected: true, eligible: true, room: true, score: 1, checks: [] });
  const legs = Object.entries(obj(doc.jobs)).flatMap(([id, j]) => {
    const job = obj(j);
    const m = obj(obj(job.strategy).matrix);
    const axes = Object.values(m).filter(Array.isArray) as unknown[][];
    const n = axes.reduce((k, a) => k * a.length, 1);
    const labels = strs(job["runs-on"]);
    return Array.from({ length: n }, (_, i) => ({
      key: `${id}#${i}`,
      job: id,
      name: n > 1 ? `${id} (${i + 1})` : id,
      matrix: {},
      runs_on: { labels },
      routing: { labels, image: null, executors: ["baremetal"], runners: labels.includes("macos") ? [fit("metal-mac-mini")] : [fit("fedora"), fit("adacachy")] },
    }));
  });
  const schedules = (Array.isArray(obj(doc.on).schedule) ? (obj(doc.on).schedule as unknown[]) : []).map(obj).map((s) => {
    const tz = s.timezone ? String(s.timezone) : null;
    const next = nextDaily(String(s.cron), tz);
    return {
      cron: String(s.cron),
      timezone: tz,
      skip_if_unchanged: s["skip-if-unchanged"] === true,
      catch_up: s["catch-up"] === true,
      jitter: s.jitter ? String(s.jitter) : null,
      inputs: obj(s.inputs),
      next,
      error: next ? null : `the mock reads daily crons only, not ${String(s.cron)}`,
    };
  });
  return {
    repo: body.repo,
    ref: "main",
    sha: "",
    path: body.path,
    event: body.event,
    runs: true,
    why: null,
    diagnostics: [],
    plan: { name: String(doc.name ?? ""), legs, decided: [], removed: {}, later: [] },
    schedules,
  };
}

/** Commits the mock knows tags name today, for KCI6002's fix. */
const PINS: Record<string, string> = { "azure/setup-helm@v4": "1a275c3f0b9e8d7c6b5a4f3e2d1c0b9a8f7e6d5c" };

/**
 * `POST /api/ci/validate` (W02, as the editor asks it): text that isn't YAML is an error where it
 * stops reading; a third party's action followed by a tag is KCI6002, which this mock's repositories
 * require pinned (a warning), with its fix where the mock knows the commit.
 */
function validate(body: Obj): unknown {
  const text = String(body.content ?? "");
  const diagnostics: unknown[] = [];
  const doc = parseDocument(text);
  for (const e of doc.errors) {
    const [line, col] = [e.linePos?.[0]?.line ?? 1, e.linePos?.[0]?.col ?? 1];
    diagnostics.push({ code: "KCI1001", severity: "error", message: e.message.split("\n")[0], path: body.path, range: { start: { line, col }, end: { line, col: col + 1 } } });
  }
  text.split("\n").forEach((l, i) => {
    const m = /uses:\s*([^\s#@]+)@([^\s#]+)/.exec(l);
    if (!m || !m[1] || !m[2] || /^(kiln\/|\.\/|hyper-light\/|actions\/)/.test(m[1]) || /^[0-9a-f]{40}$/.test(m[2])) return;
    const at = (m.index ?? 0) + m[0].length - m[2].length + 1;
    const range = { start: { line: i + 1, col: at }, end: { line: i + 1, col: at + m[2].length } };
    const sha = PINS[`${m[1]}@${m[2]}`];
    diagnostics.push({
      code: "KCI6002",
      severity: "warning",
      message: `\`${m[1]}@${m[2]}\` follows a tag or branch: pin it to a full commit SHA so it can't change under you`,
      path: body.path,
      range,
      ...(sha ? { fix: { title: `Pin it to ${sha.slice(0, 7)}`, edits: [{ range, text: sha }] } } : {}),
    });
  });
  return { kind: String(body.path ?? "").endsWith("action.yml") ? "action" : "workflow", dialect: String(body.path ?? "").startsWith(".kiln/") ? "kiln" : "gha", diagnostics, outline: null };
}

/** What each browser sent `POST /api/ship/pipelines`, by its user agent. */
const sent = new Map<string, Record<string, unknown>[]>();
const bodiesOf = (ua: string) => {
  let b = sent.get(ua);
  if (!b) {
    b = [];
    sent.set(ua, b);
  }
  return b;
};

export async function handlePipelines(req: http.IncomingMessage, res: http.ServerResponse, url: URL, ctx: Ctx): Promise<boolean> {
  const ua = String(req.headers["user-agent"] ?? "");
  const p = url.pathname;
  const m = req.method ?? "GET";
  if (p === "/api/mock/ship/pipelines") {
    if (url.searchParams.get("reset")) sent.delete(ua);
    ctx.json(res, 200, { bodies: bodiesOf(ua) });
    return true;
  }
  if (p === "/api/ship/pipelines" && m === "GET") {
    ctx.json(res, 200, list(url.searchParams.get("q") ?? ""));
    return true;
  }
  const one = m === "GET" ? p.match(/^\/api\/ship\/pipelines\/([^/]+)\/([^/]+)$/) : null;
  if (one) {
    const repo = `${decodeURIComponent(one[1] ?? "")}/${decodeURIComponent(one[2] ?? "")}`;
    const path = url.searchParams.get("path") ?? "";
    const id = url.searchParams.get("draft");
    // A draft is opened by its id (the server's `draft_pane`); without one, the published file.
    const row = repoOf(repo).rows.find((r) => r.path === path && (id ? r.draft?.id === id : !r.draft));
    if (!SEEDS[repo] || !row) ctx.json(res, 404, { error: `no workflow or action ${path} in ${repo} you may read` });
    else ctx.json(res, 200, paneOf(repo, row));
    return true;
  }
  const labels = m === "GET" ? p.match(/^\/api\/ci\/repos\/(.+)\/labels$/) : null;
  if (labels) {
    ctx.json(res, 200, { labels: LABELS });
    return true;
  }
  if (p === "/api/ci/validate" && m === "POST") {
    ctx.json(res, 200, validate(await readJson(req)));
    return true;
  }
  if (p === "/api/ci/plan-preview" && m === "POST") {
    ctx.json(res, 200, preview(await readJson(req)));
    return true;
  }
  if (p === "/api/ship/pipelines" && m === "POST") {
    const body = await readJson(req);
    bodiesOf(ua).push(body);
    ctx.json(res, 200, { ok: true, verb: body.verb ?? null });
    return true;
  }
  return false;
}
