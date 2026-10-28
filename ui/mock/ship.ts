/**
 * The mock's Ship (crates/kiln/src/ship.rs's `GET /api/ship/commits`, docs/ci.md W26-R01, R02):
 * kiln's own repository with D41 under review (a question of kim's, its author's to answer), a
 * change request with a commit folded under its head, a GitHub, a GitLab and a Bitbucket pull
 * request, and commits on their way to production. `host`, `kind` and `lens` filter as the server
 * does.
 */
import type http from "node:http";
import type { ShipChange, ShipCommit, ShipDetail, ShipMark, ShipPage, ShipPip } from "../src/api/ship.ts";
import { STATES } from "../src/components/ui/status-glyph-spec.ts";
import { Trunk } from "./ship-sim.ts";

interface Ctx {
  json: (res: http.ServerResponse, status: number, body: unknown) => void;
  /** One of the mock's own jobs, running if one is (server.ts), for a fixed row's leg to stream. */
  liveJob?: () => { run: string; job: string; name: string; state: string } | null;
  /** CI starting on a pushed commit (server.ts's run on kiln's repository): its run and job events reach the page as a real push's do. */
  push?: () => void;
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

/** The fixed rows' times: the mock's start, so every read of the page sends the same rows (W26-R15's memo reads "the same" by them). */
const T0 = now();

function commits(): ShipCommit[] {
  const t = T0;
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
      change: { ...change("D41", "diff", "kiln", "pipeline: dynamic matrices", "requested", ["user:ada"], { question: 1, nit: 0, issue: 0, blocking: 0 }), revision: 7, latest: 7 },
      runs: [{ id: "run-41", pipeline: "ci" }],
      actions: [
        { name: "hand_off", method: "POST", path: "/api/attachments", body: { repo: REPO, diff: "D41" } },
        { name: "review", method: "GET", path: "/api/diffs/D41", body: null },
      ],
    }),
    // A landed change, GitLab's !6: its tip commit carries its review, approved and landed (W26-R14's
    // e2e); older than D41 so the newest is still D41, on a sha of its own.
    row({
      sha: sha("6"),
      short: "6666666",
      message: "ci: a landed change",
      author: "user:ada",
      at: t - 3600,
      stand: "shipped",
      line: "Landed · In production",
      cursor: 12,
      pips: pips("approved", "passed", "passed", "passed", "passed", "healthy"),
      kind: "request",
      host: "gitlab",
      environments: [{ environment: "production", production: true }],
      change: { ...change("!6", "request", "gitlab", "ci: a landed change", "landed", [], { question: 0, nit: 0, issue: 0, blocking: 0 }), revision: 2, latest: 2 },
      runs: [{ id: "run-44", pipeline: "ci" }],
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

/**
 * The fixed rows' runners, replanting onto "ci: a landed change", the newest in production (the
 * commit page's Replant; Build and replant plan, step 2): one replanted, one Pi whose upgrade timed
 * out until System's Retry starts it again, one offline.
 */
const PI = "pi-kitchen";
/** Retried, by browser (its User-Agent), so the e2e projects running side by side each have their own Pi. */
const piRetried = new Set<string>();
const replantOf = (row: ShipCommit, ua: string) =>
  row.short === "6666666"
    ? [
        { name: "metal-ryzen-01", state: "on", why: "" },
        { name: PI, state: piRetried.has(ua) ? "waiting" : "failed", why: piRetried.has(ua) ? "" : "the upgrade timed out: it didn't come back on the new build within 10 minutes" },
        { name: "metal-nuc-old", state: "offline", why: "offline since 3d" },
      ]
    : null;

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

/**
 * A burst of agents' commits (`POST /api/mock/ship/agents?n=14&over=10`; Agent pace and checks,
 * step 1): first a release production waits on you to approve, then agents' commits that ride with
 * it (waiting, so they fold under it), every fourth one an agent asking you. Each lands on top of
 * `arrived`, spread over `over` seconds, and starts CI as a push does (`push`); `arrive?clear=1`
 * takes them away and stops what's to come.
 */
const burstTimers = new Set<ReturnType<typeof setTimeout>>();
const agentCommit = (k: number): ShipCommit => {
  const id = sha(`a${k.toString(16)}`);
  const base = {
    repo: REPO,
    sha: id,
    short: id.slice(0, 7),
    branch: "main",
    at: now(),
    environments: [],
    tags: [],
    runs: [],
    cursor: 200 + k,
    kind: "commit" as const,
    host: null,
    change: null,
    fold_under: null,
  };
  if (k === 1)
    return {
      ...base,
      message: "release: production waits on you",
      author: "agent:release",
      stand: "needs_you",
      line: "In staging and healthy · production waits on your approval",
      pips: pips("none", "passed", "passed", "passed", "healthy", "needs"),
      actions: [
        { name: "approve", method: "POST", path: "/api/deploy/releases/ship-rel-burst/approve", body: {} },
        { name: "hold", method: "POST", path: "/api/deploy/releases/ship-rel-burst/reject", body: {} },
      ],
    };
  if (k % 4 === 0)
    return {
      ...base,
      message: `agent: asks about flake ${k}`,
      author: `agent:fix-${k}`,
      stand: "needs_you",
      line: `agent:fix-${k} asks: retry the flaky leg?`,
      pips: pips("none", "passed", "failed"),
      actions: [{ name: "hand_off", method: "POST", path: "/api/attachments", body: { repo: REPO, run: `ship-run-burst-${k}` } }],
    };
  return {
    ...base,
    message: `agent: rider ${k}`,
    author: `agent:fix-${k}`,
    stand: "waiting",
    line: "Passed and built · rides with the release",
    pips: pips("none", "passed", "passed", "passed", "healthy"),
    actions: [],
  };
};

/**
 * Every state a glyph has, on Ship's own journey strips (`POST /api/mock/ship/glyphs`; Agent pace and
 * checks, step 3, its grayscale test): commits whose seven pips run through them all, each landing,
 * so none folds; `arrive?clear=1` takes them away.
 */
const sampler = (): ShipCommit[] =>
  Array.from({ length: Math.ceil(STATES.length / 7) }, (_, i) => {
    const s = (j: number) => STATES[(i * 7 + j) % STATES.length] ?? "none";
    const id = sha(`f${i}`);
    return {
      repo: REPO,
      sha: id,
      short: id.slice(0, 7),
      message: `glyphs ${i + 1}`,
      author: "agent:glyphs",
      branch: "main",
      at: now(),
      stand: "landing" as const,
      line: `${s(0)}, ${s(1)}, ${s(2)}, ${s(3)}, ${s(4)}, ${s(5)}, ${s(6)}`,
      pips: pips(s(0), s(1), s(2), s(3), s(4), s(5), s(6)),
      environments: [],
      tags: [],
      runs: [],
      cursor: 300 + i,
      kind: "commit" as const,
      host: null,
      change: null,
      fold_under: null,
      actions: [],
    };
  });

/**
 * A fixed row's review marks, oldest first (W26-R14): a commit with no change has one, its push; a
 * change has a push per revision, a comment per open thread, and its approval, asked or given.
 */
/** The marks Review's step 8 tells its story with (W26-R14): D41 pushed, a blocking thread
 * resolved, fixed by Apply, its conflict resolved, approved; the landed !6; CR12 asked of kim. */
const MARKS: Record<string, (at: number) => ShipMark[]> = {
  D41: (at) => [
    { id: "D41-push-5", kind: "push", at: at - 3000, end: at - 3000, by: "agent:matrix", signer: null, revision: 5, state: "passed" },
    { id: "D41-comment-1", kind: "comment", at: at - 2700, end: at - 2100, by: "user:kim", signer: null, revision: 5, state: "blocked", thread: "D41-t1", note: "blocking" },
    // A resolution's mark is `<thread>:resolved`, as ship.rs makes it: its lane reads "kim · resolved".
    { id: "D41-t1:resolved", kind: "comment", at: at - 2100, end: at - 2100, by: "user:kim", signer: null, revision: 5, state: "passed", thread: "D41-t1", note: "resolved" },
    { id: "D41-fix-6", kind: "fix", at: at - 1800, end: at - 1800, by: "agent:matrix for user:ada", signer: null, revision: 6, state: "passed", thread: "D41-t1", note: "suggestion applied" },
    { id: "D41-resolution-7", kind: "resolution", at: at - 900, end: at - 900, by: "agent:matrix", signer: null, revision: 7, state: "passed" },
    { id: "D41-approval", kind: "approval", at: at - 600, end: at - 300, by: "user:kim", signer: null, revision: 7, state: "approved" },
  ],
  "!6": (at) => [
    { id: "!6-push-1", kind: "push", at: at - 2400, end: at - 2400, by: "gitlab:mei", signer: null, revision: 1, state: "passed" },
    { id: "!6-push-2", kind: "push", at: at - 1200, end: at - 1200, by: "gitlab:mei", signer: null, revision: 2, state: "passed" },
    { id: "!6-approval", kind: "approval", at: at - 900, end: at - 600, by: "gitlab:mei", signer: null, revision: 2, state: "approved" },
  ],
  CR12: (at) => [
    { id: "CR12-push-1", kind: "push", at: at - 1200, end: at - 1200, by: "user:lee", signer: null, revision: 1, state: "passed" },
    { id: "CR12-approval", kind: "approval", at: at - 600, end: null, by: "user:kim", signer: null, revision: 1, state: "requested" },
  ],
};

function marksOf(c: ShipCommit): ShipMark[] {
  const told = MARKS[c.change?.name ?? ""];
  if (told) return told(c.at);
  const base = c.at - 3600;
  const push = (n: number | null, at: number): ShipMark => ({ id: `${c.short}-push-${n ?? 0}`, kind: "push", at, end: at, by: c.author, signer: null, revision: n, state: "passed" });
  const ch = c.change;
  if (!ch) return [push(null, c.at)];
  const marks: ShipMark[] = [];
  for (let n = 1; n <= ch.revision; n++) marks.push(push(n, base + n * 600));
  const after = base + ch.revision * 600;
  const open = ch.open ? ch.open.question + ch.open.nit + ch.open.issue + ch.open.blocking : 0;
  for (let i = 0; i < open; i++)
    marks.push({
      id: `${ch.name}-comment-${i}`,
      kind: "comment",
      at: after + 60 * (i + 1),
      end: null,
      by: ch.turn[0] ?? "user:kim",
      signer: null,
      revision: ch.revision,
      state: "requested",
      thread: `${ch.name}-t${i}`,
    });
  const approved = ch.state === "approved" || ch.state === "landed";
  marks.push({
    id: `${ch.name}-approval`,
    kind: "approval",
    at: after + 300,
    end: approved ? after + 900 : null,
    by: ch.turn[0] ?? "user:lee",
    signer: null,
    revision: ch.revision,
    state: approved ? "approved" : "requested",
  });
  return marks;
}

/** Every row: those a test added, the trunk's commits (newest first), then the fixed ones. */
const all = (): ShipCommit[] => [...arrived, ...(trunk?.commits() ?? []), ...commits()];

/** Ship's routes, and the actions its rows name for the trunk's own ids (`ship-run-…`,
 * `ship-rel-…`, a trunk runner whose upgrade failed), so they answer before the routes the rest of
 * the mock keeps for those paths. True when it answered. */
export async function handleShip(req: http.IncomingMessage, res: http.ServerResponse, url: URL, ctx: Ctx): Promise<boolean> {
  const ua = String(req.headers["user-agent"] ?? "");
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
      ? {
          ...row,
          review: marksOf(row),
          checks: live
            ? [
                {
                  pipeline: "ci",
                  legs: [
                    // A row whose checks failed failed in a leg its line names ("rust lint + test · assertion failed in
                    // sync::reconnect"), which streams one of the mock's own jobs so its log opens.
                    ...(row.pips.find((x) => x.stage === "checks")?.state === "failed"
                      ? [
                          {
                            name: row.line.split(" · ")[0] ?? "checks",
                            state: "failed",
                            attempts: 2,
                            note: row.line.split(" · ").slice(1).join(" · "),
                            job: live.job,
                            run: live.run,
                            start: T0 - 900,
                            end: T0 - 840,
                          },
                        ]
                      : []),
                    {
                      name: live.name,
                      state: live.state === "running" ? "running" : "passed",
                      attempts: 1,
                      note: "",
                      job: live.job,
                      run: live.run,
                      start: T0 - 120,
                      ...(live.state === "running" ? {} : { end: T0 - 60 }),
                    },
                  ],
                },
              ]
            : [],
          ...(replantOf(row, ua) ? { replant: replantOf(row, ua) ?? [] } : {}),
          // Its runs are the mock's own (as its legs are), so their pages open (the commit page's step 3).
          runs: live ? [{ id: live.run, pipeline: "ci" }] : (row.runs ?? []),
          // A commit in production went through staging, built once: its deploys and the one image they run.
          ...(row.environments?.some((e) => e.production)
            ? {
                build: {
                  platforms: [
                    { name: "linux/amd64", state: "passed" as const },
                    // "deploy: a rollback holds" was in staging when a newer push came: its Mac build gave way to the newer one's.
                    { name: "macos/arm64", state: row.short === "1111111" ? ("superseded" as const) : ("passed" as const) },
                  ],
                  image: { state: "passed" as const, digest: `sha256:${row.sha}${row.sha}`.slice(0, 71) },
                },
                deployments: [
                  { environment: "staging", production: false, state: "success", description: "synced and healthy", at: row.at + 600 },
                  { environment: "production", production: true, state: "success", description: "promoted from staging", at: row.at + 1800 },
                ],
              }
            : {}),
        }
      : null;
    const detail: ShipDetail | null = trunk?.detail(sha ?? "") ?? fixed;
    if (!detail || detail.repo !== repo) ctx.json(res, 404, { error: "no such commit" });
    else ctx.json(res, 200, detail);
    return true;
  }
  if (p === "/api/mock/ship/agents" && m === "POST") {
    const n = Math.min(Math.max(Number(q.get("n") ?? 14) || 14, 1), 100);
    const over = Math.min(Math.max(Number(q.get("over") ?? 10) || 10, 0), 60);
    for (let k = 1; k <= n; k++) {
      const t = setTimeout(
        () => {
          burstTimers.delete(t);
          arrived.unshift(agentCommit(k));
          ctx.push?.();
        },
        ((k - 1) * over * 1000) / Math.max(1, n - 1),
      );
      burstTimers.add(t);
    }
    ctx.json(res, 200, { ok: true });
    return true;
  }
  if (p === "/api/mock/ship/glyphs" && m === "POST") {
    arrived.unshift(...sampler());
    ctx.json(res, 200, { ok: true, states: STATES.length });
    return true;
  }
  if (p === "/api/mock/ship/arrive" && m === "POST") {
    if (q.get("clear")) {
      for (const t of burstTimers) clearTimeout(t);
      burstTimers.clear();
      arrived.length = 0;
    } else arrived.unshift(arrival(arrived.length + 1));
    ctx.json(res, 200, { ok: true });
    return true;
  }
  // The fixed rows' Pi, through System's own actions: Retry (POST /api/updates/runners/:name) starts
  // its upgrade again, Diagnose (GET /api/agents/:name/diagnose) finds the timeout; a test resets it.
  if (p === `/api/updates/runners/${PI}` && m === "POST") {
    piRetried.add(ua);
    ctx.json(res, 200, { id: "upg-pi-kitchen", kind: "runner", target: PI, method: "download", state: "running", started_at: now(), ended_at: null, from_build: "0.1.0+0f1e2d3", to_build: null });
    return true;
  }
  if (p === `/api/agents/${PI}/diagnose` && m === "GET") {
    ctx.json(res, 200, {
      runner: PI,
      at: Date.now(),
      worst: "fail",
      findings: [
        {
          id: "upgrade.last",
          level: "fail",
          title: "Its last upgrade timed out",
          detail: "It went down to swap in the new build and didn't answer again within 10 minutes: a Pi on an SD card can take longer.",
          fix: { action: "retry-upgrade", label: "Upgrade it again", way: "download" },
        },
      ],
      machine_checked: false,
      why: "It hasn't answered since its upgrade.",
    });
    return true;
  }
  if (p === "/api/mock/ship/replant" && m === "POST") {
    if (q.get("reset")) piRetried.delete(ua);
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
