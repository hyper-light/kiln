/**
 * The mock's Ship (crates/kiln/src/ship.rs's `GET /api/ship/commits`, docs/ci.md W26-R01, R02):
 * kiln's own repository with D41 under review (a question of kim's, its author's to answer), a
 * change request with a commit folded under its head, a GitHub, a GitLab and a Bitbucket pull
 * request, and commits on their way to production. `host`, `kind` and `lens` filter as the server
 * does.
 */
import type http from "node:http";
import type { ShipChange, ShipCommit, ShipDetail, ShipPage, ShipPip } from "../src/api/ship.ts";
import { Trunk } from "./ship-sim.ts";

interface Ctx {
  json: (res: http.ServerResponse, status: number, body: unknown) => void;
  /** One of the mock's own jobs, running if one is (server.ts), for a fixed row's leg to stream. */
  liveJob?: () => { run: string; job: string; name: string; state: string } | null;
}

/** kiln's own trunk, simulated (ship-sim.ts); `MOCK_SHIP=static` serves the fixed rows alone, for
 * tests that need the same page every time. `MOCK_SHIP_SPEED`: simulated seconds per second. */
const trunk = process.env.MOCK_SHIP === "static" ? null : new Trunk("hyper-light/kiln", Number(process.env.MOCK_SHIP_SPEED ?? 12) || 12);

const REPO = "hyper-light/kiln";
const now = () => Math.floor(Date.now() / 1000);
const sha = (p: string) => p.repeat(40).slice(0, 40);
const pips = (review: string, land: string, checks: string, build = "none", staging = "none", production = "none", replant = "none"): ShipPip[] =>
  [
    ["review", review],
    ["land", land],
    ["checks", checks],
    ["build", build],
    ["staging", staging],
    ["production", production],
    ["replant", replant],
  ].map(([stage, state]) => ({ stage, state }));

/** The fields every row gives; the rest default as a plain commit's. */
type Row = Pick<ShipCommit, "sha" | "short" | "message" | "author" | "at" | "stand" | "line" | "pips" | "cursor"> & Partial<ShipCommit>;

function commits(): ShipCommit[] {
  const t = now();
  const change = (name: string, kind: ShipChange["kind"], host: string, title: string, state: string, turn: string[], open = { question: 0, nit: 0, issue: 0, blocking: 0 }): ShipChange => ({
    name,
    kind,
    host,
    title,
    revision: 1,
    latest: 1,
    state,
    turn,
    open,
    superseded_by: null,
  });
  const row = (o: Row): ShipCommit => ({ repo: REPO, branch: "main", environments: [], tags: [], runs: [], kind: "commit", host: null, change: null, fold_under: null, actions: [], ...o });
  return [
    row({
      sha: sha("4"),
      short: "4444444",
      message: "pipeline: dynamic matrices",
      author: "agent:matrix",
      at: t - 600,
      stand: "needs_you",
      line: "kim: empty matrix meant? · 1 thread",
      cursor: 11,
      pips: pips("requested", "none", "passed"),
      kind: "diff",
      host: "kiln",
      change: { ...change("D41", "diff", "kiln", "pipeline: dynamic matrices", "requested", ["user:ada"], { question: 1, nit: 0, issue: 0, blocking: 0 }), revision: 2, latest: 2 },
      runs: [{ id: "run-41", pipeline: "ci" }],
      actions: [
        { name: "hand_off", method: "POST", path: "/api/attachments", body: { repo: REPO, diff: "D41" } },
        { name: "review", method: "GET", path: "/api/diffs/D41", body: null },
      ],
    }),
    // D41's first revision, which its second replaced: quiet, folded under it.
    row({
      sha: sha("2"),
      short: "2222222",
      message: "pipeline: dynamic matrices",
      author: "agent:matrix",
      at: t - 3600,
      stand: "superseded",
      line: "Superseded by revision 2",
      cursor: 3,
      pips: pips("none", "none", "superseded"),
      kind: "diff",
      host: "kiln",
      fold_under: sha("4"),
    }),
    row({
      sha: sha("c"),
      short: "ccccccc",
      message: "sched: fair shares",
      author: "user:lee",
      at: t - 900,
      stand: "waiting",
      line: "Waiting on review",
      cursor: 10,
      pips: pips("requested", "none", "running"),
      kind: "request",
      host: "kiln",
      change: change("CR12", "request", "kiln", "sched: fair shares", "requested", ["user:kim"]),
    }),
    row({
      sha: sha("b"),
      short: "bbbbbbb",
      message: "sched: weights",
      author: "user:lee",
      at: t - 1000,
      stand: "waiting",
      line: "Not landed",
      cursor: 9,
      pips: pips("none", "none", "none"),
      kind: "request",
      host: "kiln",
      fold_under: sha("c"),
    }),
    row({
      sha: sha("9"),
      short: "9999999",
      message: "Faster builds",
      author: "github:ada",
      at: t - 1200,
      stand: "waiting",
      line: "Waiting on review",
      cursor: 8,
      pips: pips("requested", "none", "none"),
      kind: "request",
      host: "github",
      change: change("#5", "request", "github", "Faster builds", "requested", ["github:bob"]),
    }),
    row({
      sha: sha("7"),
      short: "7777777",
      message: "docs: the runner's limits",
      author: "gitlab:mei",
      at: t - 1300,
      stand: "waiting",
      line: "Changes asked · say what a leg may use",
      cursor: 7,
      pips: pips("changes", "none", "passed"),
      kind: "request",
      host: "gitlab",
      change: change("!7", "request", "gitlab", "docs: the runner's limits", "changes", ["gitlab:mei"]),
    }),
    row({
      sha: sha("5"),
      short: "5555555",
      message: "ci: cache keys by lockfile",
      author: "bitbucket:lee",
      at: t - 1400,
      stand: "landing",
      line: "Approved · ready to land",
      cursor: 6,
      pips: pips("approved", "none", "passed"),
      kind: "request",
      host: "bitbucket",
      change: change("#3", "request", "bitbucket", "ci: cache keys by lockfile", "approved", []),
    }),
    row({
      sha: sha("3"),
      short: "3333333",
      message: "sync: a move carries its homes",
      author: "mei",
      at: t - 1800,
      stand: "needs_you",
      line: "rust lint + test · assertion failed in sync::reconnect",
      cursor: 5,
      pips: pips("none", "passed", "failed"),
      runs: [{ id: "run-3", pipeline: "ci" }],
      actions: [
        { name: "retry", method: "POST", path: "/api/ci/runs/run-3/rerun", body: { mode: "failed" } },
        { name: "hand_off", method: "POST", path: "/api/attachments", body: { repo: REPO, run: "run-3" } },
      ],
    }),
    row({
      sha: sha("1"),
      short: "1111111",
      message: "deploy: a rollback holds",
      author: "ada",
      at: t - 7200,
      stand: "shipped",
      line: "In production · runners 2/2",
      cursor: 4,
      pips: pips("none", "passed", "passed", "passed", "healthy", "passed", "replanted"),
      environments: [{ environment: "production", production: true }],
      tags: ["v0.2.0"],
      runs: [{ id: "run-1", pipeline: "ci" }],
    }),
  ];
}

/** Commits a test adds on top (`POST /api/mock/ship/arrive`, `?clear=1` to take them away), newest first. */
const arrived: ShipCommit[] = [];
const arrival = (n: number): ShipCommit => ({
  repo: REPO,
  sha: sha(`e${n % 10}`),
  short: sha(`e${n % 10}`).slice(0, 7),
  message: `arrived ${n}`,
  author: "agent:burst",
  branch: "main",
  at: now(),
  stand: "needs_you",
  line: `arrived ${n} · waits on you`,
  pips: pips("none", "passed", "failed"),
  environments: [],
  tags: [],
  runs: [],
  cursor: 100 + n,
  kind: "commit",
  host: null,
  change: null,
  fold_under: null,
  actions: [],
});

/** Every row: those a test added, the trunk's commits (newest first), then the fixed ones. */
const all = (): ShipCommit[] => [...arrived, ...(trunk?.commits() ?? []), ...commits()];

/** Ship's routes, and the actions its rows name for the trunk's own ids (`ship-run-…`,
 * `ship-rel-…`, a trunk runner whose upgrade failed), so they answer before the routes the rest of
 * the mock keeps for those paths. True when it answered. */
export async function handleShip(req: http.IncomingMessage, res: http.ServerResponse, url: URL, ctx: Ctx): Promise<boolean> {
  const p = url.pathname;
  const m = req.method ?? "GET";
  const q = url.searchParams;
  if (p === "/api/ship/commits" && m === "GET") {
    const repo = q.get("repo");
    const list = all().filter(
      (c) => (!repo || c.repo === repo) && (!q.get("host") || c.host === q.get("host")) && (!q.get("kind") || c.kind === q.get("kind")) && (q.get("lens") !== "review" || c.kind !== "commit"),
    );
    const page: ShipPage = { commits: list, next_cursor: null };
    ctx.json(res, 200, page);
    return true;
  }
  const one = m === "GET" ? p.match(/^\/api\/ship\/commits\/(.+)\/([0-9a-f]{7,40})$/) : null;
  if (one) {
    const [, repo, sha] = one;
    const row = all().find((c) => c.repo === repo && c.sha.startsWith(sha ?? ""));
    const live = ctx.liveJob?.();
    const fixed: ShipDetail | null = row
      ? { ...row, checks: live ? [{ pipeline: "ci", legs: [{ name: live.name, state: live.state === "running" ? "running" : "passed", attempts: 1, note: "", job: live.job, run: live.run }] }] : [] }
      : null;
    const detail: ShipDetail | null = trunk?.detail(sha ?? "") ?? fixed;
    if (!detail || detail.repo !== repo) ctx.json(res, 404, { error: "no such commit" });
    else ctx.json(res, 200, detail);
    return true;
  }
  if (p === "/api/mock/ship/arrive" && m === "POST") {
    if (q.get("clear")) arrived.length = 0;
    else arrived.unshift(arrival(arrived.length + 1));
    ctx.json(res, 200, { ok: true });
    return true;
  }
  if (!trunk || m !== "POST") return false;
  const ok = (done: boolean) => (ctx.json(res, done ? 200 : 409, done ? { ok: true } : { error: "not now" }), true);
  const rel = p.match(/^\/api\/deploy\/releases\/(ship-rel-[^/]+)\/(approve|reject)$/);
  if (rel) return ok(rel[2] === "approve" ? trunk.approve(rel[1] ?? "") : trunk.hold(rel[1] ?? ""));
  const rerun = p.match(/^\/api\/ci\/runs\/(ship-run-[^/]+)\/rerun$/);
  if (rerun) return ok(trunk.retry(rerun[1] ?? ""));
  const runner = p.match(/^\/api\/updates\/runners\/([^/]+)$/);
  if (runner && trunk.failedRunner(decodeURIComponent(runner[1] ?? ""))) return ok(trunk.retryRunner(decodeURIComponent(runner[1] ?? "")));
  if (p === "/api/mock/ship/burst") {
    trunk.burst(Math.min(Number(q.get("n") ?? 14) || 14, 100));
    return ok(true);
  }
  if (p === "/api/attachments") {
    // Peek: a hand-off of one of the trunk's runs is ours; anything else goes on to the mock's
    // agents, the same bytes replayed for whoever reads the body next.
    const chunks: Buffer[] = [];
    for await (const c of req) chunks.push(c as Buffer);
    const raw = Buffer.concat(chunks);
    Object.defineProperty(req, Symbol.asyncIterator, {
      configurable: true,
      value: async function* () {
        yield raw;
      },
    });
    let run: unknown;
    try {
      run = (JSON.parse(raw.toString("utf8") || "{}") as { run?: unknown }).run;
    } catch {
      run = undefined;
    }
    if (typeof run === "string" && run.startsWith("ship-run-")) return ok(trunk.handOff(run));
  }
  return false;
}
