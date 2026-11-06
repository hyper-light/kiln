/**
 * Ship's lists taking `ship` frames in place (W21-R15, src/views/ship/live.ts): a page with each
 * frame applied is the page the server would answer next, through realistic sequences (a job
 * started, moving, finished; a run done; a review and a comment; a new commit pushed; a commit gone
 * from memory) and random ones, for the repositories' list and a repository's pages, through the
 * facets; what can't be placed loads again; an older server's page loads again on every event.
 */
import assert from "node:assert/strict";
import { test } from "node:test";
import { shipApi, type ShipCommit, type ShipDetail, type ShipPage, type ShipQuery } from "../src/api/ship.ts";
import type { ServerEvent } from "../src/api/types.ts";
import type { ShipRow } from "../src/api/wire/index.ts";
import { applyShipDetail, applyShipPage, matches, rowFor, shipCommit, shipCommits, shipOn, type ShipFrame } from "../src/views/ship/live.ts";

const row = (repo: string, n: number, extra: Partial<ShipCommit> = {}): ShipCommit => ({
  repo,
  sha: `${repo.slice(-1)}${String(n).padStart(39, "0")}`,
  short: String(n).padStart(7, "0"),
  message: `commit ${n}`,
  author: "ada",
  branch: "main",
  at: 1_800_000_000 + n,
  stand: "checking",
  line: "Checks running",
  pips: [{ stage: "checks", state: "running" }],
  cursor: n,
  kind: "commit",
  host: null,
  change: null,
  v: `v${n}`,
  ...extra,
});

/** What the server holds: every commit, as everyone sees it and as those whose turn it is see it. */
interface Held {
  rows: Map<string, { row: ShipCommit; yours?: ShipCommit; whose?: string[] }>;
}

const key = (c: ShipCommit) => `${c.repo}/${c.sha}`;
const seen = (h: Held, viewer: string[]) => [...h.rows.values()].map((r) => rowFor(r as unknown as ShipRow, viewer));

/** The server's answer to `q` (ship.rs's `ShipCache::page` and `heads`). */
function server(h: Held, q: ShipQuery, viewer: string[]): ShipPage {
  const all = seen(h, viewer)
    .filter((c) => (!q.repo || c.repo === q.repo) && matches(q, c))
    .sort((a, b) => b.cursor - a.cursor);
  if (q.by === "repo") {
    const heads = new Map<string, ShipCommit>();
    for (const c of all) if (!heads.has(c.repo)) heads.set(c.repo, c);
    return { commits: [...heads.values()], next_cursor: null, viewer };
  }
  const limit = q.limit ?? 30;
  const after = all.filter((c) => q.cursor === undefined || c.cursor < q.cursor);
  const page = after.slice(0, limit);
  return { commits: page, next_cursor: after.length > limit ? (page.at(-1)?.cursor ?? null) : null, viewer };
}

/** Changes the server's rows and says them as frames, one a repository, as a refresh does. */
function change(h: Held, rows: { row: ShipCommit; yours?: ShipCommit; whose?: string[] }[], gone: ShipCommit[] = []): ShipFrame[] {
  const by = new Map<string, ShipFrame>();
  const frame = (repo: string) => by.get(repo) ?? by.set(repo, { type: "ship", repo, rows: [] }).get(repo)!;
  for (const r of rows) {
    h.rows.set(key(r.row), r);
    frame(r.row.repo).rows.push(r as unknown as ShipRow);
  }
  for (const g of gone) {
    h.rows.delete(key(g));
    const f = frame(g.repo);
    f.gone = [...(f.gone ?? []), g.sha];
  }
  return [...by.values()];
}

/** A page taken through `frames`: the next page, or undefined when it loads again. */
function through(q: ShipQuery, page: ShipPage, frames: ShipFrame[]): ShipPage | undefined {
  let p: ShipPage | undefined = page;
  for (const f of frames) {
    if (p === undefined) return undefined;
    p = applyShipPage(q, p, f as ServerEvent);
  }
  return p;
}

const QUERIES: ShipQuery[] = [
  { by: "repo", limit: 100 },
  { by: "repo", kind: "diff", limit: 100 },
  { by: "repo", repo: "acme/a", limit: 100 },
  { repo: "acme/a", limit: 5 },
  { repo: "acme/a", limit: 5, kind: "diff" },
  { repo: "acme/a", limit: 5, host: "kiln" },
  { repo: "acme/a", limit: 5, lens: "review" },
  { repo: "acme/a", limit: 200 },
];

/** Every query's page through `frames` is the server's next, or loads again; how many loaded. */
function holds(h: Held, before: Map<ShipQuery, ShipPage>, frames: ShipFrame[], viewer: string[], what: string): number {
  let loads = 0;
  for (const q of QUERIES) {
    const page = before.get(q);
    assert.ok(page, "a page before");
    const got = through(q, page, frames);
    const want = server(h, q, viewer);
    if (got === undefined) loads++;
    else assert.deepEqual(got.commits, want.commits, `${what}: ${JSON.stringify(q)}`);
    if (got !== undefined) assert.equal(got.next_cursor, want.next_cursor, `${what}: ${JSON.stringify(q)}'s next page`);
    before.set(q, want);
  }
  return loads;
}

function fresh(viewer: string[]) {
  const h: Held = { rows: new Map() };
  for (let n = 1; n <= 12; n++) {
    const c = row("acme/a", n);
    h.rows.set(key(c), { row: c });
  }
  for (let n = 101; n <= 103; n++) {
    const c = row("acme/b", n);
    h.rows.set(key(c), { row: c });
  }
  const before = new Map(QUERIES.map((q) => [q, server(h, q, viewer)] as const));
  return { h, before };
}

test("w21_r15 a job started, moving and finished, and its run done: each list takes the row in place", () => {
  const viewer = ["user:ada"];
  const { h, before } = fresh(viewer);
  const at = (n: number) => h.rows.get(key(row("acme/a", n)))!.row;
  let loads = 0;
  loads += holds(h, before, change(h, [{ row: { ...at(12), line: "linux running", ran: { from: 1, to: null }, machines: ["fedora"], v: "s1" } }]), viewer, "started");
  loads += holds(h, before, change(h, [{ row: { ...at(12), line: "linux running · 2 of 3", v: "s2" } }]), viewer, "moving");
  loads += holds(
    h,
    before,
    change(h, [{ row: { ...at(12), stand: "building", line: "Checks passed", pips: [{ stage: "checks", state: "passed" }], ran: { from: 1, to: 90 }, v: "s3" } }]),
    viewer,
    "finished",
  );
  // An older commit's run (on a later page, and under the repositories' head): nothing loads.
  loads += holds(h, before, change(h, [{ row: { ...at(2), stand: "shipped", v: "s4" } }]), viewer, "an older run done");
  assert.equal(loads, 0, "every change placed, none loaded again");
});

test("w21_r15 a review and a comment: the row its viewer holds the turn of is theirs", () => {
  const ada = ["user:ada"];
  const { h, before } = fresh(ada);
  const base = row("acme/a", 12, { kind: "diff", host: "kiln" });
  const theirs = { row: { ...base, stand: "waiting", line: "kim reviews", v: "o1" }, yours: { ...base, stand: "needs_you", line: "kim asks you", v: "y1" }, whose: ["user:ada"] };
  assert.equal(holds(h, before, change(h, [theirs]), ada, "review requested"), 0);
  assert.equal(before.get(QUERIES[0]!)!.commits.find((c) => c.repo === "acme/a")?.stand, "needs_you", "Ada's turn: her row");
  const kim = ["user:kim"];
  const other = fresh(kim);
  assert.equal(holds(other.h, other.before, change(other.h, [theirs]), kim, "as kim"), 0);
  assert.equal(other.before.get(QUERIES[0]!)!.commits.find((c) => c.repo === "acme/a")?.stand, "waiting", "not kim's turn: everyone's row");
  // The diff facet: a commit that became a change comes into the diff pages; a comment on it moves it.
  assert.equal(holds(h, before, change(h, [{ ...theirs, yours: { ...theirs.yours, line: "kim: one comment", v: "y2" } }]), ada, "a comment"), 0);
});

test("w21_r15 a new commit pushed: the repositories' row and the first page take it; a later page doesn't move", () => {
  const viewer: string[] = [];
  const { h, before } = fresh(viewer);
  const later: ShipQuery = { repo: "acme/a", limit: 5, cursor: 8 };
  const page2 = server(h, later, viewer);
  const frames = change(h, [{ row: row("acme/a", 13, { stand: "landing" }) }]);
  assert.equal(holds(h, before, frames, viewer, "pushed"), 0);
  assert.deepEqual(through(later, page2, frames), server(h, later, viewer), "the second page is the same");
  assert.equal(before.get(QUERIES[0]!)!.commits.find((c) => c.repo === "acme/a")?.short, "0000013");
});

test("w21_r15 a commit gone or no longer matching: placed where nothing past it would come up, loaded again where it would", () => {
  const viewer: string[] = [];
  const { h, before } = fresh(viewer);
  const one = (n: number) => h.rows.get(key(row("acme/a", n)))!.row;
  // The oldest, from the page of 200 (nothing past it) and from no page of 5's.
  const q200 = QUERIES[7]!;
  const q5 = QUERIES[3]!;
  const p200 = before.get(q200)!;
  const p5 = before.get(q5)!;
  const frames = change(h, [], [one(1)]);
  assert.deepEqual(through(q200, p200, frames)?.commits, server(h, q200, viewer).commits);
  assert.equal(through(q5, p5, frames), undefined, "past the first page of 5: whether more are past it is the server's to say");
  // One on the first page of 5, with more past it: loads again.
  assert.equal(through(q5, p5, change(h, [], [one(10)])), undefined);
  // The newest no longer a commit of its kind: the repositories' list of diffs loads again only when it showed it.
  const diffs = QUERIES[1]!;
  const d = { row: { ...one(11), kind: "diff" as const, host: "kiln", v: "d" } };
  const p = server(h, diffs, viewer);
  const made = change(h, [d]);
  const withIt = through(diffs, p, made);
  assert.deepEqual(withIt?.commits, server(h, diffs, viewer).commits, "the first diff of its repository: its row");
  assert.equal(through(diffs, withIt!, change(h, [{ row: { ...d.row, kind: "commit", v: "c" } }])), undefined, "its only diff no longer one: loads again");
});

test("w21_r15 random sequences: every page through its frames is the server's next, or loads again, rarely", () => {
  let seed = 0x5eed;
  const rnd = (n: number) => {
    seed = (seed * 1103515245 + 12345) & 0x7fffffff;
    return seed % n;
  };
  const viewer = ["user:ada"];
  const { h, before } = fresh(viewer);
  let next = 200;
  let loads = 0;
  let rounds = 0;
  for (let round = 0; round < 400; round++) {
    const all = [...h.rows.values()].map((r) => r.row);
    const pickRow = () => all[rnd(all.length)]!;
    const what = rnd(10);
    let frames: ShipFrame[];
    if (what < 6) {
      // A leg or a run moved: one or two rows, its stand and line.
      const rows = [pickRow(), pickRow()].slice(0, 1 + rnd(2)).map((c) => {
        const r: { row: ShipCommit; yours?: ShipCommit; whose?: string[] } = { row: { ...c, stand: ["checking", "building", "shipped", "failing"][rnd(4)]!, line: `l${round}`, v: `r${round}` } };
        if (rnd(3) === 0) Object.assign(r, { yours: { ...r.row, stand: "needs_you", v: `y${round}` }, whose: [rnd(2) ? "user:ada" : "user:kim"] });
        return r;
      });
      frames = change(h, rows);
    } else if (what < 8) {
      // Became a change, or stopped being one.
      const c = pickRow();
      const diff = c.kind === "commit";
      frames = change(h, [{ row: { ...c, kind: diff ? "diff" : "commit", host: diff ? "kiln" : null, v: `k${round}` } }]);
    } else if (what < 9) frames = change(h, [{ row: row(rnd(4) ? "acme/a" : "acme/b", next++) }]);
    else frames = change(h, [], [pickRow()]);
    loads += holds(h, before, frames, viewer, `round ${round}`);
    rounds += QUERIES.length;
  }
  assert.ok(loads < rounds / 5, `loaded again ${loads} times of ${rounds}`);
});

test("w21_r15 a page from a server that sends no ship frames loads again on every event; a frame of another repository changes nothing", () => {
  const old: ShipPage = { commits: [row("acme/a", 1)], next_cursor: null };
  const job = { type: "job", job: { id: "j" } } as unknown as ServerEvent;
  assert.equal(applyShipPage({ by: "repo" }, old, job), undefined);
  const now: ShipPage = { ...old, viewer: [] };
  assert.equal(applyShipPage({ by: "repo" }, now, job), now, "the server's frames say what the job changed");
  assert.equal(applyShipPage({ repo: "acme/a" }, now, { type: "ship", repo: "acme/b", rows: [{ row: row("acme/b", 9) as never }] } as ServerEvent), now);
  assert.equal(applyShipPage({ by: "repo" }, now, { type: "ship", repo: "acme/a", rows: [], pace: true } as ServerEvent), undefined, "the pace line moved: the repositories' list loads again");
  assert.equal(applyShipPage({ repo: "acme/a" }, now, { type: "ship", repo: "acme/a", rows: [], pace: true } as ServerEvent), now, "a repository's page shows no pace line");
  assert.equal(applyShipPage({ repo: "acme/a" }, now, { type: "ship", repo: "acme/a", rows: [], whole: true } as ServerEvent), undefined);
});

test("w21_r15 a commit's detail: its row's part laid over it when its dv holds, loaded again when it doesn't", () => {
  const c = row("acme/a", 5);
  const detail: ShipDetail = { ...c, checks: [{ pipeline: "ci", legs: [] }], jobs: [], dv: "d1", viewer: ["user:ada"] };
  const frame = (r: Partial<ShipRow> & { row: ShipCommit }): ServerEvent => ({ type: "ship", repo: "acme/a", rows: [r as unknown as ShipRow] }) as ServerEvent;
  const moved = { ...c, line: "moved", v: "v2" };
  assert.deepEqual(applyShipDetail(detail, frame({ row: moved, dv: "d1" })), { ...detail, line: "moved", v: "v2" });
  assert.equal(applyShipDetail(detail, frame({ row: moved, dv: "d2" })), undefined, "its checks moved");
  assert.equal(applyShipDetail(detail, frame({ row: moved })), undefined, "no dv: the server didn't follow it");
  assert.equal(applyShipDetail(detail, frame({ row: row("acme/a", 6), dv: "x" })), detail, "another commit's row");
  const yours = { ...moved, stand: "needs_you", v: "y" };
  assert.equal(applyShipDetail(detail, frame({ row: moved, yours, whose: ["user:ada"], dv: "d1" }))?.stand, "needs_you");
  assert.equal(applyShipDetail(detail, { type: "run", run: {} } as unknown as ServerEvent), detail);
  assert.equal(applyShipDetail({ ...detail, dv: undefined, viewer: undefined }, { type: "run", run: {} } as unknown as ServerEvent), undefined, "an older server's detail");
  assert.equal(applyShipDetail(detail, { type: "ship", repo: "acme/a", rows: [], gone: [c.sha] } as ServerEvent), undefined);
});

test("w21_r15 a commit's detail takes its parts in place: wholes and list entries laid over the dv they go from", () => {
  const c = row("acme/a", 5);
  const job = (id: string, verbs: string[]) => ({ id, run: "r", pipeline: "ci", name: id, verbs, why_not: {} });
  const detail = {
    ...c,
    checks: [{ pipeline: "ci", legs: [] }],
    build: { platforms: [], image: { state: "none", digest: null } },
    jobs: [job("a", ["cancel"]), job("b", ["cancel"]), job("c", ["cancel"])],
    dv: "d1",
    viewer: [],
  } as unknown as ShipDetail;
  const moved = { ...c, line: "b passed", v: "v2" };
  const frame = (r: Partial<ShipRow>): ServerEvent => ({ type: "ship", repo: "acme/a", rows: [{ row: moved, ...r } as unknown as ShipRow] }) as ServerEvent;
  const image = { platforms: [], image: { state: "passed", digest: "sha256:x" } };
  const got = applyShipDetail(
    detail,
    frame({
      dv: "d2",
      base: "d1",
      parts: { build: image },
      items: {
        jobs: {
          len: 4,
          at: [
            { i: 1, v: job("b", ["retry"]) },
            { i: 3, v: job("d", []) },
          ],
        },
      },
    }),
  );
  assert.deepEqual(got, { ...detail, line: "b passed", v: "v2", build: image, jobs: [job("a", ["cancel"]), job("b", ["retry"]), job("c", ["cancel"]), job("d", [])], dv: "d2" });
  // A list that got shorter keeps its first entries.
  assert.deepEqual(applyShipDetail(detail, frame({ dv: "d3", base: "d1", items: { jobs: { len: 1, at: [] } } }))?.jobs, [job("a", ["cancel"])]);
  assert.equal(applyShipDetail(detail, frame({ dv: "d3", base: "d0", parts: { build: image } })), undefined, "from a dv it doesn't hold: loads again");
});

test("w21_r15 Ship asks for run, leg, review ... events only while the server's answers say it sends no frames", async () => {
  const was = { commits: shipApi.commits, commit: shipApi.commit };
  try {
    assert.ok(shipOn().includes("job"), "before any answer: as before");
    shipApi.commits = async () => ({ commits: [], next_cursor: null, viewer: [] });
    await shipCommits({ by: "repo" });
    assert.deepEqual(shipOn(), ["ship"], "an answer with viewer: its frames say what changed");
    shipApi.commit = async () => ({ ...row("acme/a", 1) }) as ShipDetail;
    await shipCommit("acme/a", "a1");
    assert.ok(shipOn().includes("run"), "an answer without (a server from before frames): its events load it again");
  } finally {
    Object.assign(shipApi, was);
  }
});
