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
import { stripTypeScriptTypes } from "node:module";
import vm from "node:vm";
import { parse, parseDocument, stringify } from "yaml";
import type { ShipPipelineDraft, ShipPipelineGroup, ShipPipelineLine, ShipPipelinePane, ShipPipelineRepo, ShipPipelineRow, ShipPipelineVerb, ShipPipelines } from "../src/api/ship.ts";
import { ADDED, CALLED, CHECKS, FILES, JOB_SECS, LAST_COMMIT, LOCK_REVIEWS, LOCKS, RESOLVED, RUNS, SET_IN, TAG_AGO, TAGS, UPDATES, USED_BY } from "./pipelines-files.ts";
// The pane's words as the server words them: the UI's own, so the mock and the typed text agree.
import { triggerLines } from "../src/lib/triggers.ts";
import { jobLine, spanWords, type JobLast } from "../src/lib/typed-groups.ts";

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
        released: "v0.2.0",
        stand: "shipped",
        word: "Released",
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
        name: "bench",
        path: ".github/workflows/bench.yml",
        stand: "waiting",
        word: "Unpublished",
        line: "actions/checkout@v4 moves to 11bd719 · it runs nowhere until a person accepts it",
        pips: ["passed", "passed", "none"],
        who: "ada",
        when: ago(20 * M),
        draft: draft("d-lock-89", "unpublished", "ada", "#89"),
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
function rank(r: { word: string; draft?: ShipPipelineDraft | null }): [number, number] {
  const need = r.word === "Review" ? 0 : r.word === "Failing" ? 1 : r.word === "Can't run" ? 2 : 3;
  const draftRank = r.draft && r.draft.state !== "review" ? 1 : 2;
  return [need < 3 ? 0 : draftRank, need];
}

function rowOf(repo: string, s: Seed): ShipPipelineRow {
  const head = HEADS[repo] ?? { ref: "main", commit: "0".repeat(40) };
  const key = s.draft ? `${repo}:${s.path}:draft:${s.draft.id}` : `${repo}:${s.path}`;
  const row: Omit<ShipPipelineRow, "v"> = { ...s, key, ref: head.ref, commit: LAST_COMMIT[key] ?? head.commit, released: s.released ?? null, draft: s.draft ?? null };
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

/** The drafts New made, by the browser that made them (its user agent), each in its repository. */
const madeBy = new Map<string, { repo: string; seed: Seed; files: { path: string; text: string }[] }[]>();

function repoOf(name: string, ua = ""): ShipPipelineRepo {
  const base = SEEDS[name] ?? { access: "none", rows: [] };
  const seed = { ...base, rows: [...base.rows, ...(madeBy.get(ua) ?? []).filter((m) => m.repo === name).map((m) => m.seed)] };
  const sorted = [...seed.rows].sort((a, b) => {
    const [ra, na] = rank(a);
    const [rb, nb] = rank(b);
    return ra - rb || na - nb || a.name.toLowerCase().localeCompare(b.name.toLowerCase()) || a.path.localeCompare(b.path) || (a.draft ? 0 : 1) - (b.draft ? 0 : 1);
  });
  const { say, needs } = heading(seed.rows, seed.access !== "none");
  const head = HEADS[name] ?? { ref: "main", commit: "0".repeat(40) };
  return { repo: name, say, needs, access: seed.access, ref: head.ref, commit: head.commit, rows: sorted.map((s) => rowOf(name, s)) };
}

function list(q: string, ua: string): ShipPipelines {
  const words = q.trim().toLowerCase();
  const repos = Object.keys(SEEDS)
    .map((r) => repoOf(r, ua))
    .map((r) => (words && !r.repo.toLowerCase().includes(words) ? { ...r, rows: r.rows.filter((x) => [x.name, x.path, x.kind, x.word].some((v) => v.toLowerCase().includes(words))) } : r))
    .filter((r) => r.rows.length > 0 || !words);
  // W29-R02, pl-1: each repository follows its most urgent row, in the same order as rows.
  const urgency = (r: ShipPipelineRepo) =>
    r.rows.reduce((best, row) => {
      const [group, need] = rank(row);
      return Math.min(best, group * 4 + need);
    }, 11);
  repos.sort((a, b) => urgency(a) - urgency(b) || a.repo.localeCompare(b.repo));
  return { repos };
}

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
  const out: ShipPipelineGroup[] = [{ name: "Triggers", lines: triggerLines(text, kiln) ?? [] }];
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
  // Each job's outcome in the last finished run, and its commit, as the server reads them (pl-2's strip).
  const finished = (RUNS[key] ?? []).find((r) => r.status !== "under way");
  const lastOf = (id: string): JobLast | undefined => (finished?.sha && !finished.refused ? { outcome: finished.failed?.includes(id) ? "failed" : "passed", sha: finished.sha } : undefined);
  const stages = stagesOf(jobs);
  const calls = stages.length === 1 && Object.values(jobs).some((j) => obj(j).uses);
  stages.forEach((ids, i) =>
    out.push({
      name: calls ? "Jobs" : `Stage ${i + 1}`,
      lines: ids.map((id) => jobLine(id, obj(jobs[id]), JOB_SECS[key]?.[id], lastOf(id), CALLED[String(obj(jobs[id]).uses ?? "")])),
    }),
  );
  const seen = new Set<string>();
  const uses: ShipPipelineLine[] = [];
  const direct: [string, boolean][] = Object.values(jobs)
    .map(obj)
    .flatMap((j) => [
      ...(j.uses ? [[String(j.uses), true] as [string, boolean]] : []),
      ...(Array.isArray(j.steps)
        ? j.steps
            .map(obj)
            .filter((s) => s.uses)
            .map((s): [string, boolean] => [String(s.uses), false])
        : []),
    ]);
  const todo: [string, boolean, number][] = direct.reverse().map(([u, call]) => [u, call, 0]);
  while (todo.length) {
    const next = todo.pop();
    if (!next) break;
    const [u, isCall, depth] = next;
    if (seen.has(u)) continue;
    seen.add(u);
    if (isCall && depth < 4)
      todo.push(
        ...(CALLED[u]?.uses ?? [])
          .slice()
          .reverse()
          .map(([u, call]): [string, boolean, number] => [u, call, depth + 1]),
      );
    // As the server words it: a repository kiln hosts by its plain name too, and its standing against that repository's newest release.
    const ref = u.includes("@") ? u.slice(u.lastIndexOf("@") + 1) : "";
    const owner = u
      .replace(/^kiln:\/\//, "")
      .split("/")
      .slice(0, 2)
      .join("/");
    const hosted = Boolean(SEEDS[owner]);
    const newest = hosted && /^v?\d+\.\d+\.\d+$/.test(ref) ? (TAGS[owner] ?? []).map(([t]) => t).find((t) => /^v?\d+\.\d+\.\d+$/.test(t)) : undefined;
    const standing = newest ? (newest === ref ? "newest" : `${newest} is out`) : (RESOLVED[u] ?? "");
    uses.push({
      key: `uses.${u}`,
      kind: "use",
      name: u,
      value: `${isCall ? "a reusable workflow, " : ""}${hosted ? "a repository kiln hosts" : fromWords(u)}${standing ? ` · ${standing}` : ""}`,
      ...(newest && newest !== ref ? { state: "flag" } : {}),
    });
  }
  out.push({ name: "Uses", lines: uses });
  const names = (re: RegExp) => [...new Set([...text.matchAll(re)].map((m) => m[1] ?? ""))];
  out.push({
    name: "Secrets and variables",
    lines: [
      ...names(/secrets\.([A-Za-z_][A-Za-z0-9_]*)/g).map((n) => ({ key: `secrets.${n}`, kind: "secret", name: n, value: SET_IN[n] ?? "used" })),
      ...names(/vars\.([A-Za-z_][A-Za-z0-9_]*)/g).map((n) => ({ key: `vars.${n}`, kind: "variable", name: n, value: SET_IN[n] ?? "used" })),
    ],
  });
  out.push({
    name: "Runs",
    // As the server words them: its commit, what came of it, how long ago; its event beside it.
    lines: (RUNS[key.replace(/:draft:.*$/, "")] ?? []).map((r) => {
      const status = r.status === "failed" && r.failed?.length ? `${r.failed.join(", ")} failed` : r.status === "failed" && r.refused ? `refused (${r.refused})` : r.status;
      return {
        key: `runs.${r.n}`,
        kind: "run",
        name: r.subject ?? (r.sha ? r.sha.slice(0, 7) : `#${r.n}`),
        value: r.ago === undefined ? status : `${status} · ${spanWords(r.ago)}`,
        note: r.event,
        state: r.status === "passed" ? "passed" : r.status === "failed" ? "failed" : "running",
      };
    }),
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
      ...(doc.concurrency ? [{ key: "settings.concurrency", kind: "setting", name: "Concurrency", value: String(obj(doc.concurrency).group ?? doc.concurrency) }] : []),
      ...lockLines(LOCKS[key.replace(/:draft:.*$/, "")], jobs),
      ...(Object.keys(call).length ? [{ key: "settings.access", kind: "setting", name: "Access", value: "shared with hyper-light's repositories", state: "owner" }] : []),
    ],
  });
  return out.filter((g) => g.lines.length > 0);
}

/** A use a lock can hold, as `kiln_ci::lock::editor::lockable` says: another repository's, by a ref. */
const lockable = (u: string) => !/^(\.\/|\$\/|docker:\/\/|kiln\/)/.test(u) && u.includes("@");

/** Settings' lock lines as `kiln_ci::pipelines::LockSeen` words them (W29-R23–R26): what its locks hold, what the trunk requires, moved tags. */
function lockLines(lock: { locked: boolean; require: "off" | "report" | "enforce"; moved?: string[] } | undefined, jobs: Record<string, unknown>): ShipPipelineLine[] {
  const facts = lock ?? { locked: false, require: "off" as const };
  const uses = Object.values(jobs)
    .map(obj)
    .flatMap((j) => [...(j.uses ? [String(j.uses)] : []), ...(Array.isArray(j.steps) ? j.steps.map(obj).flatMap((st) => (st.uses ? [String(st.uses)] : [])) : [])])
    .filter(lockable);
  const n = new Set(uses).size;
  const held: ShipPipelineLine = facts.locked
    ? { key: "settings.lock", kind: "setting", name: "Locked", value: n === 0 ? "it uses nothing a lock holds" : n === 1 ? "its one use at its commit" : `${n} uses at their commits`, state: "passed" }
    : { key: "settings.lock", kind: "setting", name: "Not locked", value: "its uses follow their tags", state: "flag" };
  const require: ShipPipelineLine =
    facts.require === "off"
      ? { key: "settings.require_lock", kind: "setting", name: "Require lockfile", value: "off", state: "none" }
      : facts.require === "report"
        ? { key: "settings.require_lock", kind: "setting", name: "Require lockfile", value: "report: its runs say what isn't locked", state: facts.locked ? "passed" : "flag" }
        : { key: "settings.require_lock", kind: "setting", name: "Require lockfile", value: "enforce: what isn't locked doesn't run", state: facts.locked ? "passed" : "failed" };
  const moved = facts.moved ?? [];
  const tags: ShipPipelineLine = {
    key: "settings.moved_tags",
    kind: "setting",
    name: "Moved tags",
    value: `${moved[0]}${moved.length > 1 ? ` and ${moved.length - 1} more` : ""} moved: flagged`,
    state: "flag",
  };
  // Only what there's something to say of: Settings that says only its defaults isn't shown.
  const required = facts.require !== "off";
  return [...(facts.locked || required ? [held, require] : []), ...(moved.length ? [tags] : [])];
}

/** Used by's lines as the server words them (pl-5): each repository by its workflows and version, flagged where that's behind the newest. */
const usedByLines = (used: { repo: string; ref: string; sha: string; runs: number; workflows?: string[] }[], newest?: string): ShipPipelineLine[] =>
  used.map((c) => ({
    key: `used_by.${c.repo}@${c.ref}`,
    kind: "consumer",
    name: c.repo,
    value: c.workflows?.length ? `${c.workflows.join(", ")} · ${c.ref}` : c.ref,
    ...(newest && c.ref !== newest && /^v?\d+\.\d+\.\d+$/.test(c.ref) ? { state: "flag" } : {}),
  }));

function actionGroups(repo: string, key: string, text: string, kind: string, head: string): ShipPipelineGroup[] {
  const doc = yamlOf(text);
  // Its newest release, and how many commits since (pl-5's `3 commits since v2.4.0`).
  const newest = (TAGS[repo] ?? []).map(([t]) => t).find((t) => /^v?\d+\.\d+\.\d+$/.test(t));
  const since = SINCE[key]?.length ?? 0;
  const runs = obj(doc.runs);
  const out: ShipPipelineGroup[] = [
    {
      name: "Inputs",
      // As the server words them (pl-5): what each takes when it isn't given, one added since the newest release saying so.
      lines: Object.entries(obj(doc.inputs)).map(([n, i]) => {
        const o = obj(i);
        const added = (ADDED[key] ?? []).includes(n);
        const unless = o.description ? /;\s*(.+?) when not given$/.exec(String(o.description))?.[1] : undefined;
        const v = added && newest ? `new since ${newest}` : o.required === true ? "required" : o.default !== undefined ? String(o.default) : (unless ?? "optional");
        return { key: `inputs.${n}`, kind: "input", name: n, value: v, ...(added ? { state: "added" } : {}) };
      }),
    },
    ...(Object.keys(obj(doc.outputs)).length
      ? [{ name: "Outputs", lines: Object.entries(obj(doc.outputs)).map(([n, o]) => ({ key: `outputs.${n}`, kind: "output", name: n, value: String(obj(o).description ?? "") })) }]
      : []),
  ];
  if (kind === "composite")
    out.push({
      name: "Steps",
      // An action by what it uses and is given, a command by its name and that it runs (pl-5).
      lines: (Array.isArray(runs.steps) ? runs.steps.map(obj) : []).map((s, i) => {
        const name = String(s.uses ?? s.name ?? s.id ?? String(s.run ?? "").split("\n")[0] ?? `step ${i + 1}`);
        return {
          key: `runs.steps.${i}`,
          kind: "step",
          name,
          value: s.uses ? Object.values(obj(s.with)).map(String).join(", ") : name === String(s.run ?? "").trim() && !name.includes("\n") ? "its output" : "run",
        };
      }),
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
      // The tip by the commits since the newest release; each release by its age and users; a floating tag by what it stands for (pl-5).
      { key: "versions.head", kind: "version", name: head.slice(0, 7), value: newest ? `${since} commit${since === 1 ? "" : "s"} since ${newest}` : "" },
      ...(TAGS[repo] ?? []).map(([t, sha]) => {
        const full = /^v?\d+\.\d+\.\d+/.test(t);
        const users = new Set((USED_BY[key] ?? []).filter((c) => c.ref === t).map((c) => c.repo)).size;
        const age = TAG_AGO[`${repo}:${t}`];
        const stands = (TAGS[repo] ?? []).find(([o, s]) => o !== t && s === sha && /^v?\d+\.\d+\.\d+/.test(o))?.[0];
        const value = full
          ? [age === undefined ? null : spanWords(age), users ? `${users} ${users === 1 ? "repository" : "repositories"}` : null].filter(Boolean).join(" · ")
          : stands
            ? `the newest ${t}: ${stands}`
            : sha.slice(0, 7);
        return { key: `versions.${t}`, kind: "version", name: t, value };
      }),
    ],
  });
  const used = USED_BY[key];
  out.push({ name: "Used by", lines: used ? usedByLines(used, newest) : [{ key: "used_by", kind: "note", name: "Who uses it", value: "no other repository's runs have used it yet", state: "none" }] });
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
    ? mine || update
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
      ? on("ready", "Ready")
      : !mine
        ? dim("ready", "Ready", "Only its author marks it ready (W29-R12)")
        : d.state === "review"
          ? dim("ready", "Ready", "It's in review already (W29-R12)")
          : on("ready", "Ready");
  const discard = !d
    ? dim("discard", "Discard", "Discard is for a draft (W29-R13)")
    : update
      ? on("discard", "Discard")
      : !mine
        ? dim("discard", "Discard", "Only its author discards it (W29-R13)")
        : on("discard", "Discard");
  const out = [run, on("save_draft", "Save draft"), ready, discard];
  if (action || row.kind === "reusable")
    out.push(
      d
        ? dim("release", "Release", "A draft is released once it's published (W29-R21)")
        : {
            ...on("release", "Release"),
            asks: "Keep · Release: for the workflows that use it, input `targets` added, optional",
            tags: row.released ? nextVersions(row.released) : ["v0.1.0", "v1.0.0"],
          },
    );
  return out;
}

/** The versions Release offers after `tag`, as `kiln_ci::releases::next_versions` does for an optional input added: the minor first. */
function nextVersions(tag: string): string[] {
  const [maj, min, pat] = tag.replace(/^v/, "").split(".").map(Number);
  return [`v${maj}.${(min ?? 0) + 1}.0`, `v${maj}.${min}.${(pat ?? 0) + 1}`, `v${(maj ?? 0) + 1}.0.0`];
}

/** An action's commits since its newest release, as the server's pane says them (pl-5). */
const SINCE: Record<string, { sha: string; subject: string; author: string; at: number }[]> = {
  "hyper-light/ci-kit:.github/actions/setup-rust": [
    { sha: "e41a9c2", subject: "setup-rust: a targets input, for cross builds", author: "ada", at: ago(2 * H) },
    { sha: "b07d1e9", subject: "setup-rust: cache the registry by Cargo.lock", author: "agent:01a19350 for ada", at: ago(D) },
    { sha: "94c2f60", subject: "rust-check: clippy's warnings as annotations", author: "ada", at: ago(2 * D) },
  ],
};

const KIND_WORDS: Record<string, string> = {
  workflow: "workflow",
  reusable: "reusable workflow",
  composite: "composite action",
  javascript: "JavaScript action",
  docker: "Docker action",
  firecracker: "Firecracker action",
  shards: "Shards action",
};

/** R04's pick words for a published workflow, as the server works them out (`kiln_ci::pipelines::narrows`, pl-2): its newest finished run's failed jobs, and what its open drafts change. */
function narrowsOf(repo: string, path: string, text: string, kiln: boolean): Record<string, string[]> {
  const key = `${repo}:${path}`;
  const jobs = obj(yamlOf(text).jobs);
  const finished = (RUNS[key] ?? []).find((r) => r.status !== "under way");
  const failed = (finished?.failed ?? []).filter((id) => id in jobs).map((id) => `jobs.${id}`);
  const changed: string[] = [];
  const keep = (k: string) => {
    if (!changed.includes(k)) changed.push(k);
  };
  const usesIn = (js: Record<string, unknown>) =>
    Object.values(js)
      .map(obj)
      .flatMap((j) => [...(j.uses ? [String(j.uses)] : []), ...(Array.isArray(j.steps) ? j.steps.map(obj).flatMap((st) => (st.uses ? [String(st.uses)] : [])) : [])]);
  const said = (l: ShipPipelineLine) => JSON.stringify([l.name, l.value, l.note ?? ""]);
  for (const d of (SEEDS[repo]?.rows ?? []).filter((r) => r.path === path && r.draft)) {
    const theirs = FILES[`${key}:draft:${d.draft?.id ?? ""}`] ?? "";
    const on = new Map((triggerLines(theirs, kiln) ?? []).map((l) => [l.key, said(l)]));
    for (const l of triggerLines(text, kiln) ?? []) if (on.get(l.key) !== said(l)) keep(l.key);
    const dj = obj(yamlOf(theirs).jobs);
    for (const id of Object.keys(jobs)) if (JSON.stringify(jobs[id]) !== JSON.stringify(dj[id])) keep(`jobs.${id}`);
    const du = usesIn(dj);
    for (const u of usesIn(jobs)) if (!du.includes(u)) keep(`uses.${u}`);
  }
  return { "failed last run": failed, "changed in a draft": changed };
}

/** A row's pane: its head, the strip, its groups as the server makes them, and its file. */
/** Who accepted which lock-change draft's code (W29-R25), by the browser that asked: each test its own. */
const acceptedBy = new Map<string, Set<string>>();

function paneOf(repo: string, row: ShipPipelineRow, ua = ""): ShipPipelinePane {
  const key = row.draft ? `${repo}:${row.path}:draft:${row.draft.id}` : `${repo}:${row.path}`;
  const text = FILES[key] ?? "";
  // A row's last file change is independent of the branch tip its pane opens.
  const tip = row.draft ? row.commit : (HEADS[repo]?.commit ?? row.commit);
  const action = !["workflow", "reusable"].includes(row.kind);
  const kiln = row.dialect === "kiln";
  // A code workflow's groups are what its program makes (W29-R27).
  const code = !action && isCode(row.path);
  const program = code ? made(row.path, text) : null;
  const shape = program && "workflow" in program ? stringify(program.workflow) : code ? "" : text;
  const groups = action ? actionGroups(repo, key, text, row.kind, tip) : workflowGroups(key, shape, kiln || code, row.stand !== "superseded");
  const update = UPDATES[key];
  // kiln's update draft in its ordinary groups: the use it moves in Uses, under the version it had, marked (`kiln_ci::pipelines::mark_moved`, pl-7).
  if (update)
    for (const l of groups.find((g) => g.name === "Uses")?.lines ?? [])
      if (l.key === `uses.${update.moves}@${update.update.to}`) {
        l.name = `${update.moves}@${update.update.from}`;
        l.value = `${l.value.split(" · ")[0] ?? ""} · → ${update.update.to} in ${row.draft?.change?.name ?? "the draft"}`;
        l.state = "changes";
      }
  // A lock-change draft (W29-R25): the use whose locked commit it changes marked in Uses, as `kiln_ci::pipelines::mark_relocked` does; its strip as `relock_said`.
  const review = LOCK_REVIEWS[key];
  const accepted = review && acceptedBy.get(ua)?.has(review.id) ? "user:ada" : null;
  if (review)
    for (const l of groups.find((g) => g.name === "Uses")?.lines ?? []) {
      const d = review.differences[l.name];
      if (!d) continue;
      l.value = `${l.value.split(" · ")[0] ?? ""} · → ${d.after.commit.slice(0, 7)} in ${row.draft?.change?.name ?? "the draft"}`;
      l.state = "changes";
    }
  const files = review ? Object.values(review.differences).reduce((n, d) => n + d.files.length, 0) : 0;
  const relocked = review
    ? [
        ...Object.entries(review.differences).map(([u, d]) => `${u} moves to ${d.after.commit.slice(0, 7)}`),
        files === 1 ? "its code changed in 1 file" : `its code changed in ${files} files`,
        accepted ? `accepted by ${accepted.replace(/^user:/, "")}` : "it runs nowhere until a person accepts it",
      ]
    : null;
  // What releasing it changes for those who use it, in sentences, and its repository's own checks on its head (pl-5).
  const checks = CHECKS[key];
  const released: ShipPipelineLine[] = [
    SINCE[key]
      ? { key: "changes.input.targets", kind: "change", name: "a new input, targets (optional): nothing they pass changes", value: "safe", state: "added" }
      : { key: "changes", kind: "note", name: "nothing they pass changes", value: "same", state: "passed" },
    ...(checks ? [{ key: "changes.checks", kind: "note", ...checks }] : []),
  ];
  const byHand = groups.some((g) => g.name === "Triggers" && g.lines.some((l) => l.key === "on.workflow_dispatch"));
  const narrow = !action && !row.draft ? narrowsOf(repo, row.path, text, kiln) : null;
  return {
    head: { title: row.name, path: row.path, kind: row.kind, dialect: row.dialect, ref: row.ref, commit: tip, draft: row.draft, stand: row.stand, word: row.word, pips: row.pips },
    strip: {
      said: update ? update.said : (relocked ?? [`${KIND_WORDS[row.kind] ?? row.kind} · ${row.line}`]),
      strong: update?.strong ?? (review ? Object.keys(review.differences) : []),
      pick: narrow ? [...Object.keys(narrow), "all"] : ["all"],
      ...(narrow ? { narrow } : {}),
      verbs: verbsOf(row, byHand),
    },
    groups,
    detail: {
      name: row.path,
      views: action ? ["Changes", "YAML", "Try it"] : code ? ["Code", "What it makes", "Plan"] : ["YAML", "Flow", "Plan"],
      text,
      ...(row.released && !row.draft && (action || row.kind === "reusable")
        ? {
            since: { tag: row.released, commits: SINCE[key] ?? [] },
            changes: released,
          }
        : {}),
      ...(update ? { changes: [...update.changes, ...update.upstream.map(([n, v]) => ({ key: `upstream.${n}`, kind: "upstream", name: n, value: v }))], update: update.update } : {}),
      // A GitHub workflow's overlay, none yet in the mock: where Flow writes kiln's own triggers (W29-R06).
      ...(row.dialect === "gha"
        ? {
            overlay: {
              path: `.kiln/overlays/${
                row.path
                  .split("/")
                  .pop()
                  ?.replace(/\.ya?ml$/, "") ?? ""
              }.yaml`,
              text: "",
            },
          }
        : {}),
    },
    repo,
    ...(review ? { lock_review: { ...review, accepted_by: accepted } } : {}),
  };
}

/** A code workflow (W29-R27): `.kiln/<name>.ts` or `.js`, the workflow its program makes. */
const isCode = (path: string) => /^\.kiln\/[^/]+\.(ts|js)$/.test(path);

/** kiln's own keys, as the mock knows them: what GitHub has none of. */
const KILN_KEYS = ["toolchain", "resources", "requires", "image", "artifacts", "routing", "supersede", "cadence", "coalesce", "run-env", "checkout"];

/**
 * What a code workflow's program makes, as the server's QuickJS makes it (the mock's stand-in):
 * its types stripped as Node strips them (the server's erasure is held to the same), then run with
 * `@kiln/sdk`'s calls; or the code, line and message of why it makes nothing.
 */
function made(path: string, text: string, event: Obj = { ref: "refs/heads/main" }): { workflow: Obj } | { code: string; line: number; message: string } {
  let js = text;
  if (path.endsWith(".ts")) {
    try {
      js = stripTypeScriptTypes(text, { mode: "strip" });
    } catch (e) {
      const line = text.split("\n").findIndex((l) => /^\s*(export\s+)?(const\s+)?enum\b|\bnamespace\b|import\s+\w+\s*=/.test(l)) + 1;
      return {
        code: "KCI1033",
        line: Math.max(line, 1),
        message: /\benum\b/.test(text.split("\n")[line - 1] ?? "") ? "an `enum` isn't erasable TypeScript: use a `const` object" : String((e as Error).message),
      };
    }
  }
  const body = js.replace(/import\s*\{([^}]*)\}\s*from\s*["']@kiln\/sdk["'];?/g, (_, names: string) => `const {${names}} = __sdk;`).replace(/export\s+default\s+/, "__out.v = ");
  const sdk = { workflow: (w: unknown) => w, job: (j: unknown) => j, step: (x: unknown) => x, event, tree: { read: () => null, list: () => [] } };
  const out: { v?: unknown } = {};
  try {
    vm.runInNewContext(body, { __sdk: sdk, __out: out }, { timeout: 1000, filename: path });
    const v = typeof out.v === "function" ? (out.v as (e: Obj) => unknown)(event) : out.v;
    return { workflow: obj(JSON.parse(JSON.stringify(v ?? null))) };
  } catch (e) {
    const at = new RegExp(`${path.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}:(\\d+)`).exec(String((e as Error).stack ?? ""));
    return { code: "KCI1030", line: Number(at?.[1] ?? 1), message: String((e as Error).message) };
  }
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

/** A GitHub workflow's overlay's schedule entries (W02-R07); none where it isn't YAML. */
function overlaySchedules(text: string): Obj[] {
  try {
    const o = obj(obj(parse(text)).on).schedule;
    return Array.isArray(o) ? o.map(obj) : [];
  } catch {
    return [];
  }
}

/** `POST /api/ci/plan-preview` (W04-R29), as the mock plans: a job's legs by its matrix, where linux legs would run. */
function preview(body: Obj): unknown {
  const text = String(body.content ?? FILES[`${String(body.repo)}:${String(body.path)}`] ?? "");
  let doc: Obj;
  let emitted: { emitted: string; emitted_github: boolean } | null = null;
  if (isCode(String(body.path ?? ""))) {
    const event = obj(body.event).name ?? body.event ?? "push";
    const m = made(String(body.path), text, {
      ref: "refs/heads/main",
      ...(event === "pull_request" ? { pull_request: { number: 1 } } : {}),
      ...(event === "workflow_dispatch" ? { inputs: obj(obj(body.event).inputs) } : {}),
    });
    if (!("workflow" in m)) {
      const at = { line: m.line, col: 1 };
      return {
        repo: body.repo,
        ref: "main",
        sha: "",
        path: body.path,
        event: body.event,
        runs: false,
        why: "the workflow has errors",
        diagnostics: [{ code: m.code, severity: "error", message: m.message, path: "", range: { start: at, end: at } }],
        plan: null,
      };
    }
    doc = m.workflow;
    const yaml = stringify(doc);
    emitted = { emitted: yaml, emitted_github: !KILN_KEYS.some((k) => new RegExp(`^\\s*${k}:`, "m").test(yaml)) };
  } else
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
  // A GitHub workflow's overlay gives its schedules kiln's keys, by their cron (W02-R07).
  const over = overlaySchedules(String(body.overlay ?? ""));
  const withOverlay = (s: Obj): Obj => ({ ...s, ...obj(over.find((o) => String(o.cron) === String(s.cron))) });
  const schedules = (Array.isArray(obj(doc.on).schedule) ? (obj(doc.on).schedule as unknown[]) : [])
    .map(obj)
    .map(withOverlay)
    .map((s) => {
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
    ...(emitted ?? {}),
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
  if (isCode(String(body.path ?? ""))) {
    const m = made(String(body.path), text);
    if (!("workflow" in m)) diagnostics.push({ code: m.code, severity: "error", message: m.message, path: "", range: { start: { line: m.line, col: 1 }, end: { line: m.line, col: 2 } } });
    return { kind: "workflow", dialect: "kiln", diagnostics, outline: null };
  }
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
      // W29-R25's "lock it", beside the fix: the use kept as written, its commit in the draft's locks.
      lock: { uses: `${m[1]}@${m[2]}` },
    });
  });
  return { kind: String(body.path ?? "").endsWith("action.yml") ? "action" : "workflow", dialect: String(body.path ?? "").startsWith(".kiln/") ? "kiln" : "gha", diagnostics, outline: null };
}

/** New's files as `kiln_ci::scaffold` makes them (the mock's stand-in): a workflow's file, or an action's action.yml and what it runs. */
function scaffoldOf(body: Obj): { path: string; files: { path: string; text: string }[] } {
  const kind = String(body.kind ?? "workflow");
  const name = String(body.name ?? "new");
  const kiln = body.language === "kiln";
  if (kind === "workflow" && body.language === "ts") {
    const path = `.kiln/${name}.ts`;
    const text = `// ${name}: a kiln workflow in TypeScript (docs/ci.md W29-R27). Pipelines shows the YAML it makes.\nimport { workflow, job, step } from "@kiln/sdk";\n\nexport default workflow({\n  name: ${JSON.stringify(name)},\n  on: { push: { branches: ["main"] } },\n  jobs: {\n    ${JSON.stringify(name)}: job({\n      "runs-on": ["baremetal"],\n      steps: [\n        step({ uses: "kiln/checkout@v1" }),\n        step({ run: 'echo "it runs"' }),\n      ],\n    }),\n  },\n});\n`;
    return { path, files: [{ path, text }] };
  }
  if (["workflow", "reusable"].includes(kind)) {
    const path = kiln ? `.kiln/${name}.yaml` : `.github/workflows/${name}.yml`;
    const copy = obj(body.from).copy;
    if (typeof copy === "string") return { path, files: [{ path, text: (FILES[`hyper-light/kiln:${copy}`] ?? "").replace(/^name: .*$/m, `name: ${name}`) }] };
    const on = kind === "reusable" ? "on:\n  workflow_call:\n" : kiln ? "on:\n  push:\n    branches: [main]\n" : "on:\n  push:\n    branches: [main]\n  workflow_dispatch:\n";
    const steps = kiln ? "      - uses: kiln/checkout@v1\n" : "      - uses: actions/checkout@v4\n";
    return { path, files: [{ path, text: `name: ${name}\n${on}jobs:\n  ${name}:\n    runs-on: ${kiln ? "[baremetal]" : "ubuntu-latest"}\n    steps:\n${steps}      - run: echo "it runs"\n` }] };
  }
  const dir = `.github/actions/${name}`;
  const head = `name: ${name}\ndescription: What ${name} does\ninputs:\n  target:\n    description: what it works on\n    required: false\n    default: "."\n`;
  const run = `#!/bin/sh\nset -eu\necho "${name}: $1"\n`;
  const image = (script: string) => `FROM alpine:3.20\nCOPY ${script} /${script}\nRUN chmod +x /${script}\nENTRYPOINT ["/${script}"]\n`;
  const files: Record<string, { path: string; text: string }[]> = {
    composite: [{ path: `${dir}/action.yml`, text: `${head}runs:\n  using: composite\n  steps:\n    - run: echo "${name}"\n      shell: bash\n` }],
    javascript: [
      { path: `${dir}/action.yml`, text: `${head}runs:\n  using: node24\n  main: dist/index.js\n` },
      { path: `${dir}/src/index.ts`, text: `console.log("${name}");\n` },
      { path: `${dir}/package.json`, text: `{ "name": "${name}", "private": true }\n` },
    ],
    docker: [
      { path: `${dir}/action.yml`, text: `${head}runs:\n  using: docker\n  image: Dockerfile\n` },
      { path: `${dir}/Dockerfile`, text: image("entrypoint.sh") },
      { path: `${dir}/entrypoint.sh`, text: run },
    ],
  };
  const vm = (using: string) => [
    { path: `${dir}/action.yml`, text: `${head}runs:\n  using: ${using}\n  image: Dockerfile\n  resources: { cpus: 2, memory: 4Gi }\n  egress: []\n` },
    { path: `${dir}/Dockerfile`, text: image("run.sh") },
    { path: `${dir}/run.sh`, text: run },
  ];
  return { path: `${dir}/action.yml`, files: files[kind] ?? vm(kind === "shards" ? "shards" : "firecracker") };
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

let emit: (ev: unknown) => void = () => {};
/** How the mock tells the pages a repository's rows changed (`pipelines { repo }`, W29-R03). */
export function setPipelinesEmit(fn: (ev: unknown) => void) {
  emit = fn;
}

export async function handlePipelines(req: http.IncomingMessage, res: http.ServerResponse, url: URL, ctx: Ctx): Promise<boolean> {
  const ua = String(req.headers["user-agent"] ?? "");
  const p = url.pathname;
  const m = req.method ?? "GET";
  if (p === "/api/mock/ship/pipelines") {
    if (url.searchParams.get("reset")) {
      sent.delete(ua);
      madeBy.delete(ua);
      acceptedBy.delete(ua);
    }
    ctx.json(res, 200, { bodies: bodiesOf(ua) });
    return true;
  }
  // A repository's rows gathered again, as a run, a job or a push makes the server do: the pages ask again.
  if (p === "/api/mock/ship/pipelines/tick" && m === "POST") {
    emit({ type: "pipelines", repo: url.searchParams.get("repo") ?? "hyper-light/kiln" });
    ctx.json(res, 200, { ok: true });
    return true;
  }
  if (p === "/api/ship/pipelines" && m === "GET") {
    ctx.json(res, 200, list(url.searchParams.get("q") ?? "", ua));
    return true;
  }
  const one = m === "GET" ? p.match(/^\/api\/ship\/pipelines\/([^/]+)\/([^/]+)$/) : null;
  if (one) {
    const repo = `${decodeURIComponent(one[1] ?? "")}/${decodeURIComponent(one[2] ?? "")}`;
    const path = url.searchParams.get("path") ?? "";
    const id = url.searchParams.get("draft");
    // A draft is opened by its id (the server's `draft_pane`); without one, the published file.
    const row = repoOf(repo, ua).rows.find((r) => r.path === path && (id ? r.draft?.id === id : !r.draft));
    if (!SEEDS[repo] || !row) ctx.json(res, 404, { error: `no workflow or action ${path} in ${repo} you may read` });
    else ctx.json(res, 200, paneOf(repo, row, ua));
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
    if (body.verb === "new") {
      const made = scaffoldOf(body);
      if (body.dry === true) {
        // W29-R08, pl-6: a preview from the same facts as the server's, without making a draft.
        ctx.json(res, 200, { ...made, machines: { firecracker: ["fedora", "adacachy"], shards: [] }, next_version: "v2.5.0", draft_name: `#${18 + (madeBy.get(ua)?.length ?? 0)}` });
        return true;
      }
      bodiesOf(ua).push(body);
      const files = Array.isArray(body.files) ? (body.files as { path: string; text: string }[]) : made.files;
      const id = `d-new-${(madeBy.get(ua)?.length ?? 0) + 1}`;
      const action = !["workflow", "reusable"].includes(String(body.kind));
      const seed: Seed = {
        kind: String(body.kind),
        dialect: action ? null : body.language === "ts" ? "ts" : body.language === "kiln" ? "kiln" : "gha",
        name: String(body.name),
        path: made.path,
        stand: "waiting",
        word: "Unpublished",
        line: `${String(body.kind)} · no trigger runs it and no repository can use it yet`,
        pips: ["passed", "passed", "none"],
        who: "ada",
        when: Math.floor(Date.now() / 1000),
        draft: draft(id, "unpublished", "ada", `#${18 + (madeBy.get(ua)?.length ?? 0)}`),
      };
      madeBy.set(ua, [...(madeBy.get(ua) ?? []), { repo: String(body.repo), seed, files }]);
      FILES[`${String(body.repo)}:${made.path}:draft:${id}`] = files[0]?.text ?? "";
      ctx.json(res, 200, { path: made.path, files: files.map((f) => f.path), draft: { id, name: seed.draft?.change?.name, kind: "change", state: "draft", path: made.path } });
      return true;
    }
    bodiesOf(ua).push(body);
    // Lock it of a use whose source can't be read: refused as the server refuses it, before any draft (W29-R25).
    const missing = body.verb === "lock" ? /uses:\s*(octo-org\/[^\s#]+)/.exec(String(body.content ?? ""))?.[1] : undefined;
    if (missing) {
      ctx.json(res, 422, { error: `KCI4051 ${missing}: its source can't be read, so it isn't locked and nothing is saved` });
      return true;
    }
    // A person's word on a lock-change draft's upstream code, as its head and evidence stand (W29-R25).
    if (body.verb === "accept") {
      const review = Object.entries(LOCK_REVIEWS).find(([k]) => k.endsWith(`:draft:${String(body.draft)}`))?.[1];
      if (!review || body.evidence_sha256 !== review.evidence_sha256) {
        ctx.json(res, 409, { error: "the upstream code evidence changed; review the current revision" });
        return true;
      }
      acceptedBy.set(ua, new Set([...(acceptedBy.get(ua) ?? []), review.id]));
    }
    // cache-cargo's checks failed at its head: Release says so, unless it's asked anyway (W29-R21).
    if (body.verb === "release" && String(body.path).endsWith("cache-cargo") && body.confirm !== true) {
      ctx.json(res, 409, { error: "ci-kit's checks failed on e41a9c2: rust-check · clippy." });
      return true;
    }
    ctx.json(res, 200, { ok: true, verb: body.verb ?? null });
    return true;
  }
  return false;
}
