/**
 * The mock's Ship (crates/kiln/src/ship.rs's `GET /api/ship/commits`, docs/ci.md W26-R01, R02):
 * kiln's own repository with D41 under review (a question of kim's, its author's to answer), a
 * change request with a commit folded under its head, a GitHub, a GitLab and a Bitbucket pull
 * request, and commits on their way to production. `host`, `kind` and `lens` filter as the server
 * does.
 */
import type http from "node:http";

interface Ctx {
  json: (res: http.ServerResponse, status: number, body: unknown) => void;
}

const REPO = "hyper-light/kiln";
const now = () => Math.floor(Date.now() / 1000);
const sha = (p: string) => p.repeat(40).slice(0, 40);
const pips = (review: string, land: string, checks: string, build = "none", staging = "none", production = "none", replant = "none") =>
  [
    ["review", review],
    ["land", land],
    ["checks", checks],
    ["build", build],
    ["staging", staging],
    ["production", production],
    ["replant", replant],
  ].map(([stage, state]) => ({ stage, state }));

function commits() {
  const t = now();
  const change = (name: string, kind: string, host: string, title: string, state: string, turn: string[], open = { question: 0, nit: 0, issue: 0, blocking: 0 }) => ({
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
  const row = (o: Record<string, unknown>) => ({ repo: REPO, branch: "main", environments: [], tags: [], runs: [], kind: "commit", host: null, change: null, fold_under: null, actions: [], ...o });
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
      change: change("D41", "diff", "kiln", "pipeline: dynamic matrices", "requested", ["user:ada"], { question: 1, nit: 0, issue: 0, blocking: 0 }),
      runs: [{ id: "run-41", pipeline: "ci" }],
      actions: [
        { name: "hand_off", method: "POST", path: "/api/attachments", body: { repo: REPO, diff: "D41" } },
        { name: "review", method: "GET", path: "/api/diffs/D41", body: null },
      ],
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

export function handleShip(req: http.IncomingMessage, res: http.ServerResponse, url: URL, ctx: Ctx): boolean {
  if (url.pathname !== "/api/ship/commits" || (req.method ?? "GET") !== "GET") return false;
  const q = url.searchParams;
  const list = commits().filter((c) => (!q.get("host") || c.host === q.get("host")) && (!q.get("kind") || c.kind === q.get("kind")) && (q.get("lens") !== "review" || c.kind !== "commit"));
  ctx.json(res, 200, { commits: list, next_cursor: null });
  return true;
}
