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
import type { ShipPipelineDraft, ShipPipelinePane, ShipPipelineRepo, ShipPipelineRow, ShipPipelines } from "../src/api/ship.ts";

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

/** A row's pane: its head, and the anatomy the server sends for it. */
function paneOf(repo: string, row: ShipPipelineRow): ShipPipelinePane {
  return {
    head: { title: row.name, path: row.path, kind: row.kind, dialect: row.dialect, ref: row.ref, commit: row.commit, draft: row.draft, stand: row.stand, word: row.word, pips: row.pips },
    strip: { said: [], pick: [], verbs: [] },
    groups: [],
    detail: { name: row.path, views: [], text: "" },
    repo,
  };
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
  if (p === "/api/ship/pipelines" && m === "POST") {
    const body = await readJson(req);
    bodiesOf(ua).push(body);
    ctx.json(res, 200, { ok: true, verb: body.verb ?? null });
    return true;
  }
  return false;
}
