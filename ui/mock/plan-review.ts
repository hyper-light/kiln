/**
 * Plans waiting on a person's review, in the mock (views/plans/review.tsx, docs/plans.md "Reviewing a
 * plan"), answered as the server answers them:
 *
 *   /agents/rev-cicd   "CI/CD Integration" (Devin): its goal stack waits on its goal 6's plan, as
 *                      kiln keeps it (5 steps, research of 7 findings, one graded C)
 *   /agents/rev-big    "Planner audit" (Claude Code): its goal stack waits on a plan with heavy
 *                      research (46 findings from 31 sources: weak, contested and unquoted ones among
 *                      them, a step resting on 12, one on none)
 *   /agents/rev-mode   "Slates" (Claude Code): it proposes a plan in plan mode, its harness asking
 *
 *   GET  /api/plans/:id, /versions; GET /api/plans?session=   their plans (one read alone with `reviewed`)
 *   POST /api/plans/:id/review   {answer: approve|reject|revise, words?, version?, key}
 *   POST /api/plans/:id/run      while its goals wait on its review: approved instead, as the server does
 *   GET  /api/research/:id; GET /api/research?agent=          their research
 *   GET  /api/harness/sessions/:id/goals; POST …/goals/start  their goal stacks
 *
 * Revise plays the agent changing the plan: a few seconds later it's back for review, a version on,
 * with what changed. Other… on a proposed plan plays the same: it proposes again, changed.
 */
import type http from "node:http";
import type { WebSocket } from "ws";
import type { AgentInfo, Approval, GoalItem, GoalStack, HEvent, State } from "../src/api/harness.ts";
import type { Plan, PlanItem, PlanReviewed, PlanVersion } from "../src/api/plans.ts";
import type { Finding, Research, ResearchSource } from "../src/api/research.ts";
import { defaultInfo, defaultMeta, defaultState } from "./defaults.ts";
import { PROPOSED } from "./pages.ts";
import { planSteps } from "../src/views/plans/steps.ts";

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

let emitEvent: (ev: unknown) => void = () => {};
export function setReviewEmit(fn: (ev: unknown) => void) {
  emitEvent = fn;
}

const now = () => Date.now();
const secs = (ms: number) => Math.floor(ms / 1000);
const T0 = now() - 2 * 3600_000;
const CWD = "/home/ada/src/kiln";

/* ------------------------------------------------------------------ research */

const grade = (score: number) => (score >= 85 ? "A" : score >= 70 ? "B" : score >= 50 ? "C" : score >= 30 ? "D" : "F") as Finding["confidence"]["grade"];
const WORDS = { A: "high", B: "good", C: "moderate", D: "low", F: "very low" } as const;

interface Said {
  claim: string;
  score: number;
  /** Sources cited, by id; the first quote's words. */
  cites: string[];
  quote?: string;
  why?: string;
  /** A counterclaim, and how much it weighs. */
  against?: [string, number];
  /** A quote not found where it's cited. */
  unfound?: boolean;
}

function finding(id: string, s: Said, sources: ResearchSource[]): Finding {
  const g = grade(s.score);
  const failed = s.unfound ? 1 : 0;
  const cites = s.cites.map((src, i) => ({
    source: src,
    quote: i === 0 ? s.quote : undefined,
    check: { state: (i === 0 && s.unfound ? "not_found" : "found") as "found" | "not_found", at: T0 + 600_000, said: i === 0 && s.unfound ? "the page doesn't hold those words" : undefined },
  }));
  const kinds = [...new Set(s.cites.map((c) => sources.find((x) => x.id === c)?.type ?? "web"))];
  return {
    id,
    claim: s.claim,
    cites,
    against: s.against ? [{ claim: s.against[0], cites: [{ source: s.cites[0] ?? "s1", check: { state: "found", at: T0 } }], strength: s.against[1] >= 0.3 ? "moderate" : "weak" }] : [],
    at: T0 + 300_000,
    confidence: {
      score: s.score,
      grade: g,
      words: WORDS[g],
      support: s.score / 100,
      against: s.against?.[1] ?? 0,
      reliability: s.score >= 70 ? 0.9 : 0.5,
      citation: failed ? 0.3 : 1,
      independent: s.cites.length,
      cites: s.cites.length,
      found: s.cites.length - failed,
      failed,
      why:
        s.why ??
        `${s.cites.length} independent source${s.cites.length === 1 ? "" : "s"} (${kinds.join(", ")}); ${s.cites.length - failed} of ${s.cites.length} quotes found where cited${failed ? `, ${failed} not` : ""}${s.against ? `; 1 counterclaim, weighing ${Math.round(s.against[1] * 100)}%` : ""}`,
    },
    against_weights: s.against ? [s.against[1]] : [],
  };
}

function research(id: string, plan: string, agent: string, question: string, sources: ResearchSource[], said: Said[]): Research {
  const findings = said.map((s, i) => finding(`f${i + 1}`, s, sources));
  const grades: Research["summary"]["grades"] = {};
  for (const f of findings) grades[f.confidence.grade] = (grades[f.confidence.grade] ?? 0) + 1;
  const inside = sources.filter((s) => ["measurement", "code", "data", "log", "doc", "media", "record"].includes(s.type)).length;
  return {
    id,
    question,
    agent,
    by: `agent:${agent}`,
    plan,
    sources,
    findings,
    skipped: {},
    summary: { findings: findings.length, grades, sources: sources.length, internal: inside, external: sources.length - inside, checking: 0 },
    created_at: T0,
    updated_at: T0 + 900_000,
  };
}

const src = (id: string, type: ResearchSource["type"], title: string, locator: string, published?: string): ResearchSource => ({
  id,
  type,
  title,
  locator,
  retrieved_at: T0,
  ...(published ? { published } : {}),
});

// CI/CD Integration's goal 6, as kiln keeps it (plan 01a17bd748ff…, research 01a17bd74904…).
const CICD_GOAL =
  "Opened commit, Changes (plan 01a175b7f21b7455bcdcd71e94ecdce0): under the lanes, the commit's diff with line comments as GitHub's and kiln's own review show it, reusing ui/src/views/review (FileDiff, threads, composer) rather than copying it: files fold with status, +/- and Viewed; a line number opens a comment under it; threads inline with Reply and Resolve; the Diff button goes to it; the lanes card's width at every size; Playwright comments, replies and resolves at 1280 and 390.";
const CICD_SOURCES = [
  src("s1", "code", "kiln's review: a file of a diff", "ui/src/views/review/diff.tsx"),
  src("s2", "code", "kiln's review page", "ui/src/views/review/page.tsx"),
  src("s3", "code", "review threads and Viewed marks", "crates/kiln/src/review.rs"),
  src("s4", "doc", "docs/review.md", "docs/review.md"),
  src("s5", "doc", "Ship and Review's boundary", "~/.cache/kiln-review/NOTES.md"),
  src("s6", "record", "the opened-commit plan", "kiln plan 01a175b7f21b7455bcdcd71e94ecdce0"),
  src("s7", "docs", "Reviewing proposed changes in a pull request", "https://docs.github.com/en/pull-requests/collaborating-with-pull-requests/reviewing-changes-in-pull-requests", "2025"),
];
const CICD_RESEARCH = research("rs_cicd", "pl_cicd", "rev-cicd", CICD_GOAL, CICD_SOURCES, [
  {
    claim:
      "kiln's review already draws a file of a diff with its header (where it is, how it changed, viewed), its hunks, threads in place under their lines, and a comment that starts from a line's number, so Ship can reuse it rather than copy it.",
    score: 90,
    cites: ["s1", "s2"],
    quote: "export function FileDiff({",
  },
  { claim: "FileDiff takes Viewed, comment, reply and resolve as callbacks, so a host page can wire it to its own data.", score: 90, cites: ["s1", "s4"], quote: "onViewed?: (path: string) => void" },
  {
    claim: "Review's threads and Viewed marks are kept per diff (/api/diffs/:key/...), so a commit with no change has none until Review adds a commit's own review read.",
    score: 90,
    cites: ["s3", "s4"],
    quote: "/api/diffs/:key/threads",
  },
  {
    claim: "kiln's review page replies by posting a comment to the thread and resolves by setting the comment's state, the calls Ship's reuse would make.",
    score: 90,
    cites: ["s2", "s3"],
    quote: 'state: "resolved"',
  },
  {
    claim:
      "Ship and Review agreed that the diff with its threads and composer is Review's Files, first in every item's dropdown, and that a commit with no change shows its diff from its parent; a second diff under the lanes would duplicate it.",
    score: 70,
    cites: ["s5"],
    quote: "Files is the diff: first in every item's dropdown",
  },
  {
    claim: "The opened-commit plan itself puts the item's Files (with its Conversation) first in the dropdown, built by Review's plan, not a separate Changes section under the lanes.",
    score: 65,
    cites: ["s6"],
    quote: "Files, then Conversation",
  },
  {
    claim: "On GitHub a reviewed file is marked Viewed to fold it, and a comment starts from a line (its comment icon, or its number for a range), as GitHub's own docs show.",
    score: 88,
    cites: ["s7"],
    quote: "you can mark the file as viewed",
  },
]);

// Planner audit: a plan with a lot of research behind it.
const BIG_QUESTION =
  "The planner, researched and audited: newer planning techniques read at their sources, papers and the leading engines' code (DuckDB, DataFusion, Umbra), measured against ours, and the best of them planned as steps of their own";
const BIG_SOURCES: ResearchSource[] = [
  src("s1", "code", "the cost model", "crates/kiln-db/src/plan/cost.rs"),
  src("s2", "code", "the join graph", "crates/kiln-db/src/plan/graph.rs"),
  src("s3", "code", "the stats collector", "crates/kiln-db/src/stats/analyze.rs"),
  src("s4", "code", "the executor's pipelines", "crates/kiln-db/src/exec/pipeline.rs"),
  src("s5", "code", "the plan regression harness", "crates/kiln-db/tests/plans.rs"),
  src("s6", "code", "the catalog", "crates/kiln-db/src/catalog.rs"),
  src("s7", "measurement", "JOB on the bench box, today's planner", "cargo bench -p kiln-db --bench job"),
  src("s8", "measurement", "JOB plan times", "cargo bench -p kiln-db --bench job -- --plan-only"),
  src("s9", "measurement", "analyze on JOB's data", "kiln-db analyze --all (3.7 GB)"),
  src("s10", "measurement", "the sketch prototype", "cargo bench -p kiln-db --bench analyze --features hll"),
  src("s11", "measurement", "DuckDB 1.4 on JOB", "duckdb -c '.read job/all.sql'"),
  src("s12", "measurement", "DataFusion 49 on JOB", "datafusion-cli -f job/all.sql"),
  src("s13", "log", "the bench box's runs, last 30 days", "~/.cache/kiln-db/bench/history.log"),
  src("s14", "log", "the ten worst plans' EXPLAIN ANALYZE", "~/.cache/kiln-db/audit/worst-ten.txt"),
  src("s15", "data", "JOB's data set", "~/data/imdb-job/"),
  src("s16", "data", "a 1% sample of JOB's data", "~/data/imdb-job-1pct/"),
  src("s17", "doc", "the planner's design notes", "docs/kiln-db/planner.md"),
  src("s18", "doc", "the audit's notes", "~/.cache/kiln-db/audit/NOTES.md"),
  src("s19", "record", "kiln-db's CI runs", "kiln runs hyper-light/kiln-db (last 50)"),
  src("s20", "record", "the conversation that set the audit", "kiln conversation 01a19c"),
  src("s21", "code", "DuckDB's join order optimizer", "duckdb/src/optimizer/join_order/plan_enumerator.cpp"),
  src("s22", "code", "DuckDB's distinct statistics", "duckdb/src/storage/statistics/distinct_statistics.cpp"),
  src("s23", "code", "DataFusion's join selection", "datafusion/physical-optimizer/src/join_selection.rs"),
  src("s24", "code", "DataFusion's statistics", "datafusion/common/src/stats.rs"),
  src("s25", "paper", "Moerkotte & Neumann, Dynamic Programming Strikes Back (DPhyp)", "https://doi.org/10.1145/1376616.1376672", "2008"),
  src("s26", "paper", "Leis et al., How Good Are Query Optimizers, Really?", "https://www.vldb.org/pvldb/vol9/p204-leis.pdf", "2015"),
  src("s27", "paper", "Neumann & Radke, Adaptive Optimization of Very Large Join Queries", "https://doi.org/10.1145/3183713.3183733", "2018"),
  src("s28", "paper", "Kersten et al., Tidy Tuples and Flying Start (Umbra)", "https://doi.org/10.1007/s00778-020-00643-4", "2021"),
  src("s29", "docs", "PostgreSQL: row estimation examples", "https://www.postgresql.org/docs/current/row-estimation-examples.html"),
  src("s30", "paper", "Flajolet et al., HyperLogLog", "https://algo.inria.fr/flajolet/Publications/FlFuGaMe07.pdf", "2007"),
  src("s31", "blog", "Re-planning mid-query, measured", "https://example.dev/replanning-mid-query", "2024"),
];
const BIG_SAID: Said[] = [
  { claim: "On the 113 JOB queries, our planner's plans run 2.7× slower at the median than DuckDB's on the same data and box.", score: 93, cites: ["s7", "s11"], quote: "median 2.71x" },
  { claim: "Our planner's ten worst JOB plans take 61% of the suite's total run time.", score: 91, cites: ["s7", "s14"], quote: "worst ten: 61.2% of total" },
  { claim: "Planning takes under 4 ms for every JOB query today, so a slower search has room.", score: 90, cites: ["s8"], quote: "max plan 3.8 ms" },
  { claim: "Our cost model prices a join by its inputs' row counts alone, with no distinct counts.", score: 96, cites: ["s1", "s17"], quote: "fn join_cost(left: Rows, right: Rows)" },
  { claim: "HyperLogLog sketches estimate distinct counts within 2% using about 1.5 KB a column.", score: 94, cites: ["s30", "s22"], quote: "standard error of 1.04/sqrt(m)" },
  { claim: "The stats collector already reads every column once per analyze, where a sketch can ride along.", score: 92, cites: ["s3"], quote: "for col in table.columns()" },
  { claim: "Join estimates that use distinct counts cut JOB's q-error by about 10× in the literature.", score: 86, cites: ["s26"], quote: "errors of up to 10^4" },
  { claim: "PostgreSQL estimates an equi-join's selectivity as 1/max(ndv) of its two sides.", score: 95, cites: ["s29"], quote: "1/max(num_distinct1, num_distinct2)" },
  {
    claim: "Sketch merges stay exact under concurrent inserts, so analyze needs no lock.",
    score: 41,
    cites: ["s18"],
    quote: "merges are exact",
    why: "1 independent source (a document, the agent's own notes); nothing outside agrees",
  },
  { claim: "DuckDB keeps HyperLogLog distinct counts per column, updated as rows are inserted.", score: 92, cites: ["s22"], quote: "HyperLogLog log;" },
  { claim: "DataFusion's statistics carry distinct counts, but its join reordering ignores them.", score: 84, cites: ["s23", "s24"], quote: "distinct_count: Precision<usize>" },
  {
    claim: "Correlated columns make 1/max(ndv) underestimate joins badly on JOB's movie_info.",
    score: 72,
    cites: ["s26", "s14"],
    quote: "independence assumption",
    against: ["The error on movie_info comes from skew in info_type_id, not from correlation.", 0.35],
  },
  { claim: "Analyze runs in 40 s on JOB's 3.7 GB today.", score: 90, cites: ["s9"], quote: "analyze: 40.2 s" },
  { claim: "Adding sketches to analyze costs under 8% of its run time in a prototype.", score: 88, cites: ["s10"], quote: "+7.4%" },
  { claim: "Distinct counts kept in the catalog survive restarts with one new column.", score: 87, cites: ["s6"], quote: "ALTER TABLE stats ADD COLUMN" },
  { claim: "DPhyp enumerates connected subgraphs without cross products, in time linear in the plans it emits.", score: 97, cites: ["s25"], quote: "csg-cmp-pairs" },
  { claim: "DuckDB enumerates join orders with DPhyp and falls back to a greedy order past 12 relations.", score: 94, cites: ["s21"], quote: "if (relations > 12)" },
  { claim: "Our join graph builder already gives the hypergraph DPhyp needs.", score: 85, cites: ["s2"], quote: "struct Hyperedge" },
  { claim: "JOB's largest queries join 17 relations.", score: 98, cites: ["s15", "s26"], quote: "up to 17 joins" },
  { claim: "A greedy order (GOO) stays within 2× of optimal on JOB's largest queries.", score: 78, cites: ["s27"], quote: "within a factor of 2" },
  { claim: "A greedy order planned in under 1 ms for 17 relations in our prototype.", score: 86, cites: ["s8"], quote: "goo: 0.7 ms" },
  { claim: "Umbra's adaptive compilation picks its plan before it compiles, from the same cost model.", score: 82, cites: ["s28"], quote: "adaptive execution" },
  {
    claim: "Re-planning mid-query gains 30% on JOB.",
    score: 22,
    cites: ["s31"],
    quote: "a 30% win on JOB",
    unfound: true,
    why: "1 independent source (a blog); 0 of 1 quotes found where cited, 1 not",
  },
  { claim: "Our executor can stop a pipeline at a breaker and go on with another plan.", score: 76, cites: ["s4"], quote: "fn suspend_at_breaker" },
  { claim: "Each of the ten worst plans misorders a join with movie_info or cast_info.", score: 93, cites: ["s14"], quote: "movie_info" },
  { claim: "Each of the ten worst plans reproduces on a 1% sample of the data.", score: 89, cites: ["s16", "s14"], quote: "1pct: same plan" },
  { claim: "Our plan regression harness compares plans by shape, not by cost.", score: 91, cites: ["s5"], quote: "assert_same_shape" },
  { claim: "Golden plans checked in as text diff cleanly in review.", score: 75, cites: ["s5", "s19"], quote: "plans/*.txt" },
  { claim: "DuckDB 1.4 runs JOB in 6.1 s on the bench box.", score: 92, cites: ["s11"], quote: "total 6.1 s" },
  { claim: "DataFusion 49 runs JOB in 11.8 s on the bench box.", score: 92, cites: ["s12"], quote: "total 11.8 s" },
  { claim: "The bench box's results vary by under 3% from run to run.", score: 55, cites: ["s13"], quote: "stddev 2.1%", unfound: true },
  { claim: "A planner change's checks are kiln-db's tests and the JOB bench, in CI.", score: 90, cites: ["s19", "s17"], quote: "cargo test -p kiln-db" },
  { claim: "DB2's LEO corrects estimates from past runs' actual cardinalities.", score: 74, cites: ["s26"], quote: "learning optimizer" },
  { claim: "SQL Server's estimator assumes predicates are independent unless told otherwise.", score: 71, cites: ["s26"], quote: "independence" },
  { claim: "Lookahead in a greedy order helps little past three levels.", score: 63, cites: ["s27"], quote: "diminishing returns" },
  { claim: "Bushy plans win on 4 of JOB's 113 queries.", score: 80, cites: ["s26"], quote: "bushy trees" },
  { claim: "Learned cost models such as Bao need a GPU at plan time to stay fast.", score: 58, cites: ["s26"], quote: "inference latency" },
  { claim: "TPC-H barely exercises join ordering: its queries join at most 8 relations.", score: 90, cites: ["s26"], quote: "TPC-H" },
  { claim: "DuckDB re-estimates cardinalities after filters push down.", score: 88, cites: ["s21"], quote: "EstimateCardinality" },
  { claim: "Sampling-based estimation costs more than sketches at analyze time on large tables.", score: 45, cites: ["s18"], quote: "sampling is slower" },
  { claim: "DataFusion 49 picks hash join build sides by estimated size.", score: 86, cites: ["s23"], quote: "swap_hash_join" },
  { claim: "Umbra compiles each query in under 10 ms with its fast backend.", score: 84, cites: ["s28"], quote: "Flying Start" },
  { claim: "Our CI's bench job runs on a shared machine with a noisy neighbour.", score: 66, cites: ["s19", "s13"], quote: "runs-on: [bench]" },
  { claim: "The conversation that set the audit asked for no regression on TPC-H.", score: 95, cites: ["s20"], quote: "no regression on TPC-H" },
  { claim: "kiln-db's planner was last changed in a big way 14 months ago.", score: 83, cites: ["s17", "s19"], quote: "planner rewrite" },
  { claim: "Index nested-loop joins appear in 9 of the ten worst plans.", score: 87, cites: ["s14"], quote: "IndexNLJ" },
];
const BIG_RESEARCH = research("rs_big", "pl_big", "rev-big", BIG_QUESTION, BIG_SOURCES, BIG_SAID);

/* ------------------------------------------------------------------ plans */

const crit = (text: string) => ({ kind: "qualitative" as const, text });
const item = (title: string, description: string, acceptance: string[], testing: [string, string][], basis: string[], needs?: number[]): PlanItem => ({
  title,
  description,
  examples: "",
  acceptance: acceptance.map(crit),
  testing: testing.map(([scenario, c]) => ({ scenario, criteria: [crit(c)] })),
  basis,
  ...(needs ? { needs } : {}),
});

const CICD_V2: PlanItem[] = [
  item(
    "Settle where the diff lives",
    "Ship and Review agreed that the diff with its threads and composer is Review's Files, first in every item's dropdown (a commit with no change: its diff from its parent), and the opened-commit plan says the same. A Changes section under the lanes would show that diff a second time. Before any code, ask Ada (and tell Review's agent): does Changes under the lanes replace Files' place at the top (ReviewFiles mounted under the lanes instead), or is it a second view of the same diff? Record the answer in ~/.cache/kiln-review/NOTES.md beside the boundary, and whose code builds which part.",
    ["One recorded decision says where the item's diff shows in the dropdown and who builds it, with Ada's words cited; no diff is drawn twice."],
    [["Read NOTES.md and the message thread with Review's agent", "the decision is there, cites Ada, and names the owner of each part"]],
    ["f5", "f6"],
  ),
  item(
    "Changes: the diff under the lanes",
    "Under the lanes, the item's diff drawn by kiln's review (FileDiff per file, not a copy): each file folds with its status, its +/- and Viewed (Viewed folds it, as on GitHub). A change's diff comes from its diff (/api/diffs/:key, the version shown); a commit with no change from Review's read of its diff from its parent. The lanes card's width at every size.",
    ["Each file shows its status, +/- and Viewed, and folds when marked Viewed; the section is the lanes card's width within 1 px at 1280 and 390."],
    [["Playwright in Chromium and WebKit: open a change, mark a file Viewed", "the file folds and stays marked on reload; widths match within 1 px at 1280 and 390"]],
    ["f1", "f2", "f3", "f7"],
    [1],
  ),
  item(
    "Comment, reply, resolve",
    "A line's number opens a comment under it (shift-click takes a range), posted to kiln's review threads through FileDiff's onComment; threads show inline under their lines with Reply (a comment on the thread) and Resolve (the comment's state), the calls kiln's review page makes.",
    ["A comment posted from Changes shows under its line here and on kiln's review page; a reply joins the thread; resolving folds the thread on both."],
    [["Playwright in Chromium and WebKit at 1280 and 390: comment on a line, reply, resolve", "the thread shows under the line with the reply, and resolves, in Changes and on the review page"]],
    ["f2", "f4", "f7"],
    [2],
  ),
  item(
    "The Diff button",
    "The row's Diff (Review) button opens the item and brings its Changes into view, focus on the first file, instead of leaving the page.",
    ["Diff opens the row in place and scrolls its Changes into view; the page doesn't navigate away."],
    [["Playwright: click a row's Diff button", "the row opens, Changes is in view and focused, and the URL stays /ship"]],
    ["f1", "f5"],
    [2],
  ),
  item(
    "Checks",
    "In ui/: npm run typecheck, lint, test and build; the Ship specs, the new Changes spec among them, in Chromium and WebKit at desktop and phone widths.",
    ["Every command exits 0 and every spec passes in both engines."],
    [["the commands, then E2E_WEBKIT=1 npx playwright test e2e/ship*.spec.ts", "all pass"]],
    ["f4", "f7"],
    [3, 4],
  ),
];
/** The agent's change after "Other…": step 1 rewritten as asked, step 2 moving Files rather than adding a section. */
const CICD_V3: PlanItem[] = CICD_V2.map((it, i) =>
  i === 0
    ? item(
        "Mount ReviewFiles under the lanes",
        "Files moves from the dropdown's top to under the lanes: the same ReviewFiles (FileDiff, threads, composer), mounted once. No second diff anywhere, and no question to Ada first: she answered it here.",
        ["The item's diff shows once, under the lanes; the dropdown no longer has Files."],
        [["Playwright: open an item", "exactly one diff, under the lanes"]],
        ["f1", "f5"],
      )
    : i === 1
      ? { ...it, description: `${it.description} Files leaves the dropdown in the same change.`, basis: ["f1", "f2", "f3", "f5", "f7"] }
      : it,
);

const BIG_V1: PlanItem[] = [
  item(
    "Measure today's planner on JOB",
    "Run the 113 JOB queries on the bench box with today's planner, DuckDB 1.4 and DataFusion 49 on the same data; keep each plan, its time and its estimated and actual row counts, so every later step is measured against the same numbers.",
    ["The three engines' times and today's plans for all 113 queries are kept in the audit's notes, with the bench box's load at the time."],
    [["cargo bench -p kiln-db --bench job, then the same SQL on DuckDB and DataFusion", "113 rows per engine; times within 3% across two runs"]],
    ["f1", "f2", "f3"],
  ),
  item(
    "Distinct counts in the cost model",
    "Analyze keeps a HyperLogLog sketch per column beside its row count, and the cost model estimates an equi-join's size from distinct counts (1/max(ndv), PostgreSQL's rule) instead of row counts alone. Sketches live in the catalog, so they survive restarts; analyze merges them without a lock.",
    ["Every column analyzed has a distinct count within 2% of exact on JOB's data.", "Join size q-error on JOB drops at least 5× at the median.", "Analyze takes at most 10% longer."],
    [
      ["kiln-db analyze --all on JOB's data, then compare with COUNT(DISTINCT …)", "every column within 2%"],
      ["the JOB bench with --estimates", "median q-error at least 5× lower than today's"],
      ["analyze on JOB's data, timed against today's", "at most 10% slower"],
    ],
    ["f4", "f5", "f6", "f7", "f8", "f9", "f10", "f11", "f12", "f13", "f14", "f15"],
  ),
  item(
    "Join enumeration with DPhyp",
    "Enumerate join orders with DPhyp over the hypergraph the join graph builder already gives, without cross products, for queries of up to 12 relations.",
    ["Every JOB query of up to 12 relations is planned by DPhyp; none plans a cross product."],
    [["the JOB bench with --plans", "no cross products; plan time under 50 ms each"]],
    ["f16", "f17", "f18", "f19"],
    [2],
  ),
  item(
    "A greedy order past 12 relations",
    "Past 12 relations, order joins greedily (GOO), as DuckDB does, so planning time stays bounded on JOB's largest queries.",
    ["Queries of 13 to 17 relations plan in under 5 ms and run within 2× of DPhyp's plan where DPhyp finishes."],
    [["the JOB bench, queries of 13+ relations", "plan time under 5 ms; run time within 2× of DPhyp's"]],
    ["f20", "f21"],
    [3],
  ),
  item(
    "Re-plan at run time when estimates are far off",
    "When a pipeline breaker sees actual rows 10× off the estimate, stop there and plan the rest again from what's known (Umbra's adaptive approach, in our executor's terms).",
    ["Re-planning fires only when estimates are 10× off, and JOB's total time drops at least 10%."],
    [["the JOB bench with re-planning on and off", "total at least 10% lower with it on; no query slower by more than 5%"]],
    ["f22", "f23", "f24"],
    [2],
  ),
  item(
    "The ten worst plans as regression tests",
    "Each of today's ten worst plans becomes a plan regression test on the 1% sample: its shape after the change is checked in, so a later change that brings one back fails.",
    ["Ten golden plans are checked in and fail on today's planner."],
    [["cargo test -p kiln-db --test plans", "ten tests; each fails before the change and passes after"]],
    ["f25", "f26", "f27", "f28"],
    [1],
  ),
  item(
    "Bench against DuckDB and DataFusion",
    "Run JOB again with every change in, on the same box, data and versions as step 1.",
    ["Our median JOB time is within 1.3× of DuckDB's and ahead of DataFusion's."],
    [["the JOB bench, three engines, two runs", "medians as said, runs within 3% of each other"]],
    ["f29", "f30", "f31"],
    [4, 5, 6],
  ),
  item(
    "Document the planner",
    "docs/kiln-db/planner.md: distinct counts, DPhyp and its greedy fallback, re-planning, and the regression tests, each with the numbers measured.",
    ["The doc says what each part does and cites the bench."],
    [],
    [],
    [7],
  ),
  item("Checks", "cargo test -p kiln-db, clippy, and the JOB bench in CI, green.", ["Every check passes."], [["kiln CI on the branch", "all green"]], ["f32"], [8]),
];
const BIG_V2: PlanItem[] = BIG_V1.map((it, i) =>
  i === 4 ? { ...it, basis: ["f22", "f24"], description: `${it.description} Measured on our own executor first: the blog's 30% isn't relied on.` } : it,
);

const PROPOSED_V2 = PROPOSED.replace(
  "2. Track handles the successor never saw opened",
  "2. Tests, red first: the CLI test and the coherence unit tests fail before any fix.\n3. Track handles the successor never saw opened",
)
  .replace("3. Adopt an unseen handle", "4. Adopt an unseen handle")
  .replace("4. Tests, red first: the CLI test and the coherence unit tests, then the guest tests on the box.", "5. The guest tests on the box.")
  .replace("5. Verify:", "6. Verify:");

interface Kept {
  plan: Plan;
  versions: PlanVersion[];
}

const planOf = (id: string, title: string, body: string, items: PlanItem[], session: string, agent: string, rs: string | null, goal?: { id: string; text: string }, proposed?: boolean): Kept => {
  const plan: Plan = {
    id,
    title,
    body,
    items,
    tags: goal ? ["from a goal"] : [],
    source: { session, agent, harness: session === "rev-cicd" ? "devin" : "claude", ...(goal ? { goal: { agent: session, ...goal } } : {}), ...(proposed ? { proposed: true } : {}) },
    share: null,
    version: goal ? 2 : 1,
    created_at: secs(T0),
    updated_at: secs(T0 + 1_200_000),
    steps: items.length ? items.map((i) => i.title) : [],
    runs: goal ? 1 : 0,
    phase: "ready",
    research: rs,
    reviewed: null,
  };
  const versions: PlanVersion[] = goal
    ? [
        { version: 2, title, body, items, at: secs(T0 + 1_200_000), by: `agent:${agent}` },
        { version: 1, title, body, items: [], at: secs(T0), by: "kiln" },
      ]
    : [{ version: 1, title, body, items, at: secs(T0 + 1_200_000), by: "kiln" }];
  return { plan, versions };
};

const seedPlans = () => {
  const kept = new Map<string, Kept>([
    [
      "pl_cicd",
      planOf("pl_cicd", "Opened commit, Changes (plan 01a175b7f21b7455bcdcd71e94ecdce0): under the lanes,", `Goal: ${CICD_GOAL}`, CICD_V2, "rev-cicd", "CI/CD Integration", "rs_cicd", {
        id: "g6",
        text: CICD_GOAL,
      }),
    ],
    ["pl_big", planOf("pl_big", "The planner, researched and audited", `Goal: ${BIG_QUESTION}`, BIG_V1, "rev-big", "Planner audit", "rs_big", { id: "b1", text: BIG_QUESTION })],
    ["pl_slates", planOf("pl_slates", "Fix: a mount taken over by a restarted daemon dropped a store", PROPOSED, [], "rev-mode", "Slates", null, undefined, true)],
  ]);
  kept.get("pl_slates")?.plan.steps.push(...planSteps(PROPOSED));
  return kept;
};
let plans = seedPlans();
const RESEARCH = new Map<string, Research>([
  ["rs_cicd", CICD_RESEARCH],
  ["rs_big", BIG_RESEARCH],
]);
/** Answers taken, by key: asked again, answered as things stand. */
let answered = new Set<string>();
/** What the agents do a while after an answer (change the plan, propose again): let go of on a reset. */
const timers = new Set<ReturnType<typeof setTimeout>>();
const later = (fn: () => void, ms: number) => {
  const t = setTimeout(() => {
    timers.delete(t);
    fn();
  }, ms);
  timers.add(t);
};

/* ------------------------------------------------------------------ agents */

interface Member {
  info: AgentInfo;
  events: HEvent[];
  approval?: Approval;
  stack?: { state: GoalStack["state"]; reason?: string; items: GoalItem[]; held_for?: string };
}

const base = (id: string, name: string, kind: "claude" | "devin", target: string): AgentInfo => ({
  ...defaultInfo(),
  id,
  kind,
  kind_name: kind === "claude" ? "Claude Code" : "Devin",
  target,
  name,
  cwd: CWD,
  created_at: T0,
  status: "idle",
  title: name,
  model: "claude-opus-5-5",
  mode: kind === "claude" ? "default" : "code",
  approvals: [],
  queued: 0,
  tasks_running: 0,
  cost_usd: 1.84,
  last_activity: now() - 60_000,
});

const tool = (id: string, at: number, name: string, title: string, input: unknown, output: string, extra: Record<string, unknown> = {}): HEvent =>
  ({
    id,
    seq: 0,
    at,
    kind: "tool",
    tool_id: id,
    name,
    tool: "mcp",
    server: "kiln",
    tool_short: name,
    title,
    input,
    output,
    status: "done",
    started: at,
    ended: at + 2000,
    ...extra,
  }) as unknown as HEvent;

const steps = (plan: string, items: PlanItem[], version: number, goal: string): GoalItem[] =>
  items.map((it, i) => ({
    id: `${plan}-v${version}-s${i + 1}`,
    text: `Step ${i + 1} of ${items.length} of the plan: ${it.title}`,
    status: "pending",
    part_of: goal,
    part_kind: "plan",
    step: { plan, version, n: i + 1 },
  }));

const heldWhy = (title: string) => `Paused by the agent: plan "${title}" waits on your review`;

const proposal = (id: string, plan: string, at: number): Approval => ({
  id,
  tool_id: `${id}-tool`,
  tool_name: "ExitPlanMode",
  tool: "plan",
  title: "Plan ready for review",
  input: { plan },
  plan,
  cwd: CWD,
  at,
});

function seedMembers(): Map<string, Member> {
  const members = new Map<string, Member>();
  {
    const at = now() - 6 * 60_000;
    const m: Member = {
      info: { ...base("rev-cicd", "CI/CD Integration", "devin", "local"), held: { why: heldWhy("Opened commit, Changes (plan 01a175b7…): under the lanes,"), plan: "pl_cicd", since: at } },
      events: [
        { id: "rc-u1", seq: 1, at: at - 600_000, kind: "user", text: "Work the stack. Plan each goal first, and let me see the plan before it runs." },
        tool("rc-t1", at - 300_000, "research_add", "Researched goal 6: 7 findings", { id: "rs_cicd" }, '{"id": "rs_cicd", "findings": 7}'),
        tool("rc-t2", at - 120_000, "plans_update", "Wrote its 5 steps", { id: "pl_cicd" }, '{"id": "pl_cicd", "version": 2}'),
        tool("rc-t3", at - 30_000, "plans_ready", "Readied the plan for your review", { id: "pl_cicd", then: "review" }, '{"then": "review"}'),
        {
          id: "rc-a1",
          seq: 5,
          at,
          kind: "assistant",
          text: "Goal 6's plan is ready for you: five steps, 3 and 4 side by side. Research: 7 findings, five graded A; step 1 rests on the least sure of them (f6, C), so it starts by settling where the diff lives.",
        },
      ],
      stack: {
        state: "paused",
        reason: heldWhy("Opened commit, Changes (plan 01a175b7…): under the lanes,"),
        held_for: "pl_cicd",
        items: [
          ...[
            "Indicators: kiln's UI has a StatusGlyph",
            "Commit read model: GET /api/ship/commits",
            "Mock: ui/mock serves the commit read model",
            "Ship page: /ship lists commits needs-you first",
            "Opened commit, Output: the job picked in the lanes",
          ].map((text, i): GoalItem => ({ id: `g${i + 1}`, text, status: "met", ended_at: at - (5 - i) * 600_000 })),
          { id: "g6-planning", text: 'Plan "Opened commit, Changes" (plan pl_cicd, research rs_cicd) is ready', status: "met", part_of: "g6", part_kind: "planning", reason: "the plan is ready" },
          ...steps("pl_cicd", CICD_V2, 2, "g6"),
          { id: "g6", text: CICD_GOAL, status: "pending", planned_as: "pl_cicd" },
          { id: "g7", text: "Opened commit: a row opens in place to job cards (whole cards spanning the lanes' width, paged) and lanes", status: "pending" },
          { id: "g8", text: "Commit page: /ship/{repo}/{sha} shows stage cards, the full trace with errors first", status: "pending" },
        ],
      },
    };
    members.set(m.info.id, m);
  }
  {
    const at = now() - 3 * 60_000;
    const m: Member = {
      info: { ...base("rev-big", "Planner audit", "claude", "agent:metal-epyc-01"), held: { why: heldWhy("The planner, researched and audited"), plan: "pl_big", since: at } },
      events: [
        { id: "rb-u1", seq: 1, at: at - 1_800_000, kind: "user", text: "Audit the planner against DuckDB, DataFusion and Umbra, and plan what's worth taking. Let me see the plan first." },
        tool("rb-t1", at - 900_000, "research_add", "Recorded 46 findings from 31 sources", { id: "rs_big" }, '{"id": "rs_big", "findings": 46}'),
        tool("rb-t2", at - 60_000, "plans_ready", "Readied the plan for your review", { id: "pl_big", then: "review" }, '{"then": "review"}'),
        { id: "rb-a1", seq: 4, at, kind: "assistant", text: "The plan is ready for you: nine steps on 46 findings. Two of them are weak and one is contested; step 8 rests on none." },
      ],
      stack: {
        state: "paused",
        reason: heldWhy("The planner, researched and audited"),
        held_for: "pl_big",
        items: [
          { id: "b1-planning", text: 'Plan "The planner, researched and audited" is ready', status: "met", part_of: "b1", part_kind: "planning", reason: "the plan is ready" },
          ...steps("pl_big", BIG_V1, 2, "b1"),
          { id: "b1", text: BIG_QUESTION, status: "pending", planned_as: "pl_big" },
        ],
      },
    };
    members.set(m.info.id, m);
  }
  {
    const at = now() - 2 * 60_000;
    const m: Member = {
      info: { ...base("rev-mode", "Slates", "claude", "agent:adas-macbook-pro-2.local"), status: "waiting", mode: "plan" },
      events: [
        { id: "rm-u1", seq: 1, at: at - 400_000, kind: "user", text: "Find out why the store dropped after the daemon restarted. Plan the fix first." },
        {
          id: "rm-a1",
          seq: 2,
          at: at - 20_000,
          kind: "assistant",
          text: "The successor daemon starts with a fresh handle store, so a page it re-fetches after the takeover is never tracked. Here's how I'd fix it.",
        },
        {
          id: "rm-plan1-tool",
          seq: 3,
          at,
          kind: "tool",
          tool_id: "rm-plan1-tool",
          name: "ExitPlanMode",
          tool: "plan",
          title: "Plan ready for review",
          input: { plan: PROPOSED },
          status: "running",
          started: at,
        } as unknown as HEvent,
      ],
      approval: proposal("rm-plan1", PROPOSED, at),
    };
    m.info.approvals = [{ id: "rm-plan1", tool: "plan", tool_name: "ExitPlanMode", title: "Fix: a mount taken over by a restarted daemon dropped a store", at }];
    members.set(m.info.id, m);
  }
  return members;
}
let members = seedMembers();

export const reviewInfos = (): AgentInfo[] => [...members.values()].map((m) => m.info);

/** Back as they started (`POST /api/mock/plan-review/reset`): each plan waiting on its review again. */
function reset() {
  for (const t of timers) clearTimeout(t);
  timers.clear();
  plans = seedPlans();
  members = seedMembers();
  answered = new Set();
  for (const m of members.values()) tell(m);
}

/* ------------------------------------------------------------------ live */

const viewers = new Map<string, Set<WebSocket>>();

function stateOf(m: Member): State {
  return {
    ...defaultState(),
    status: m.info.status,
    cwd: m.info.cwd,
    model: m.info.model,
    mode: m.info.mode,
    title: m.info.title,
    summary: m.info.summary,
    approvals: m.approval ? [m.approval] : [],
    usage: { cost_usd: m.info.cost_usd, input_tokens: 0, output_tokens: 0, cache_read_tokens: 0, turns: 3 },
  } as State;
}

/** One of them changed: its open pages and every list hear of it. */
function tell(m: Member, events?: HEvent[]) {
  m.info.last_activity = now();
  const state = JSON.stringify({ t: "state", state: stateOf(m) });
  for (const ws of viewers.get(m.info.id) ?? [])
    if (ws.readyState === ws.OPEN) {
      if (events?.length) ws.send(JSON.stringify({ t: "events", events }));
      ws.send(state);
    }
  emitEvent({ type: "harness", info: m.info });
}

export function handleReviewSession(ws: WebSocket, id: string): boolean {
  const m = members.get(id);
  if (!m) return false;
  const seen = viewers.get(id) ?? new Set<WebSocket>();
  viewers.set(id, seen.add(ws));
  ws.on("close", () => seen.delete(ws));
  ws.on("message", (raw) => {
    try {
      // A command under the id its client made (SessionUp), taken at once; or one as clients from before ids send it.
      const up = JSON.parse(raw.toString()) as { t?: string; id?: string; c?: unknown };
      const c = (up.t === "cmd" ? up.c : up) as { type?: string; id?: string; decision?: string; message?: string | null };
      if (up.t === "cmd" && up.id) ws.send(JSON.stringify({ t: "ack", id: up.id, outcome: "taken" }));
      if (c.type === "approve") answerProposal(m, c);
    } catch {
      // Not a command.
    }
  });
  const send = (u: unknown) => ws.send(JSON.stringify(u));
  send({ t: "open", info: m.info });
  send({ t: "meta", meta: { ...defaultMeta(), kind: m.info.kind, version: "2.3.1", features: ["attachments"], models: [], modes: [] } });
  send({ t: "state", state: stateOf(m) });
  send({ t: "events", events: m.events.map((e, i) => ({ ...e, seq: i + 1 })) });
  if (process.env.MOCK_NO_SYNCED !== "1") send({ t: "synced" });
  return true;
}

/** Its harness takes the answer to the plan it proposed: approved, it starts; denied with words, it plans again; without, it stops. */
function answerProposal(m: Member, c: { id?: string; decision?: string; message?: string | null }) {
  if (!m.approval || m.approval.id !== c.id) return;
  const at = now();
  const words = c.message?.trim();
  const toolEvent = m.events.find((e) => e.id === `${c.id}-tool`) as unknown as Record<string, unknown> | undefined;
  if (toolEvent) {
    toolEvent.status = c.decision === "deny" ? "denied" : "done";
    toolEvent.ended = at;
  }
  m.approval = undefined;
  m.info.approvals = [];
  m.info.status = c.decision === "deny" && !words ? "idle" : "running";
  m.info.mode = c.decision === "deny" ? "plan" : "default";
  tell(m, toolEvent ? [toolEvent as unknown as HEvent] : undefined);
  if (c.decision !== "deny" || !words) return;
  // It takes your words in and proposes again, changed.
  later(() => {
    const kept = plans.get("pl_slates");
    if (!kept) return;
    const t = now();
    kept.plan.version += 1;
    kept.plan.body = PROPOSED_V2;
    kept.plan.steps = planSteps(PROPOSED_V2);
    kept.plan.updated_at = secs(t);
    kept.versions.unshift({ version: kept.plan.version, title: kept.plan.title, body: PROPOSED_V2, items: [], at: secs(t), by: "kiln" });
    const id = `rm-plan${kept.plan.version}`;
    const said: HEvent = { id: `${id}-said`, seq: 0, at: t, kind: "assistant", text: "Changed as you asked: the failing tests come first now." } as HEvent;
    const ask: HEvent = {
      id: `${id}-tool`,
      seq: 0,
      at: t,
      kind: "tool",
      tool_id: `${id}-tool`,
      name: "ExitPlanMode",
      tool: "plan",
      title: "Plan ready for review",
      input: { plan: PROPOSED_V2 },
      status: "running",
      started: t,
    } as unknown as HEvent;
    m.events.push(said, ask);
    m.approval = proposal(id, PROPOSED_V2, t);
    m.info.approvals = [{ id, tool: "plan", tool_name: "ExitPlanMode", title: kept.plan.title, at: t }];
    m.info.status = "waiting";
    tell(m, [said, ask]);
  }, 4000);
}

/** A stack as the server answers it. */
const stackView = (id: string, m: Member): GoalStack | null =>
  m.stack ? { agent: id, state: m.stack.state, reason: m.stack.reason, items: m.stack.items, updated_at: now(), plan_big: true, ...(m.stack.held_for ? { held_for: m.stack.held_for } : {}) } : null;

/** Goal `goal`'s plan steps still to go, out of its stack. */
const dropSteps = (st: NonNullable<Member["stack"]>, goal: string) => {
  st.items = st.items.filter((i) => !(i.part_of === goal && i.part_kind === "plan" && (i.status === "pending" || i.status === "active")));
};

/** A person's answer to a plan its goal stack waits on (`plans.review`). */
function review(id: string, a: { answer?: string; words?: string; version?: number; key?: string }): [number, unknown] {
  const kept = plans.get(id);
  if (!kept) return [404, { error: "no such plan" }];
  const p = kept.plan;
  const goal = p.source?.goal;
  const m = goal ? members.get(goal.agent) : undefined;
  if (!goal || !m?.stack) return [400, { error: "only a plan made from a goal waits on a review here: a plan its agent proposed is answered as its harness asks" }];
  const key = a.key ? `${id} ${a.key}` : null;
  if (key && answered.has(key)) return [200, { plan: { ...p }, stack: stackView(goal.agent, m) }];
  if (m.stack.held_for !== id) return [409, { error: "nothing waits on a review of this plan now: its goals went on, or another answer came first" }];
  if (a.version !== undefined && a.version !== p.version) return [409, { error: `it changed since you looked: version ${p.version} is the one to review` }];
  const words = (a.words ?? "").trim();
  if (a.answer === "revise" && !words) return [400, { error: "say what should change (`words`)" }];
  if (!["approve", "reject", "revise"].includes(a.answer ?? "")) return [400, { error: "`answer` is approve, reject or revise" }];
  if (key) answered.add(key);
  const st = m.stack;
  const at = now();
  p.reviewed = { answer: a.answer as PlanReviewed["answer"], words: words || null, version: p.version, by: "ada", at: secs(at) };
  st.held_for = undefined;
  m.info.held = undefined;
  if (a.answer === "approve") {
    st.state = "running";
    st.reason = undefined;
    const first = st.items.find((i) => i.part_of === goal.id && i.part_kind === "plan" && i.status === "pending");
    if (first) first.status = "active";
    m.info.status = "running";
  } else if (a.answer === "reject") {
    dropSteps(st, goal.id);
    const g = st.items.find((i) => i.id === goal.id);
    if (g) Object.assign(g, { status: "skipped", reason: `Skipped by you: you rejected its plan (v${p.version})`, ended_at: at });
    st.state = "paused";
    st.reason = `Paused by you: you rejected the plan "${p.title}"`;
  } else {
    dropSteps(st, goal.id);
    p.phase = "planning";
    const part: GoalItem = {
      id: `${goal.id}-revise-${at}`,
      text: `Change the plan "${p.title}" as the person asked: "${words}"; then say it's ready again (plans_ready): it comes back to them`,
      status: "active",
      part_of: goal.id,
      part_kind: "planning",
    };
    const at6 = st.items.findIndex((i) => i.id === goal.id);
    st.items.splice(at6 < 0 ? st.items.length : at6, 0, part);
    st.state = "running";
    st.reason = undefined;
    m.info.status = "running";
    // The agent changes it, and it's back for your review, a version on.
    later(() => {
      const t = now();
      const next = id === "pl_cicd" ? CICD_V3 : id === "pl_big" ? BIG_V2 : p.items;
      p.version += 1;
      p.items = next;
      p.steps = next.map((i) => i.title);
      p.phase = "ready";
      p.updated_at = secs(t);
      kept.versions.unshift({ version: p.version, title: p.title, body: p.body, items: next, at: secs(t), by: `agent:${m.info.name}` });
      Object.assign(part, { status: "met", reason: "the plan is ready", ended_at: t });
      const g = st.items.findIndex((i) => i.id === goal.id);
      st.items.splice(g < 0 ? st.items.length : g, 0, ...steps(id, next, p.version, goal.id));
      st.state = "paused";
      st.reason = heldWhy(p.title);
      st.held_for = id;
      m.info.held = { why: heldWhy(p.title), plan: id, since: t };
      m.info.status = "idle";
      const said = { id: `${id}-v${p.version}-said`, seq: 0, at: t, kind: "assistant", text: `Changed as you asked; it's back for your review as version ${p.version}.` } as HEvent;
      m.events.push(said);
      tell(m, [said]);
    }, 5000);
  }
  tell(m);
  return [200, { plan: { ...p }, stack: stackView(goal.agent, m) }];
}

export async function handlePlanReview(req: http.IncomingMessage, res: http.ServerResponse, url: URL): Promise<boolean> {
  const p = url.pathname;
  const method = req.method ?? "GET";
  let mm: RegExpMatchArray | null;
  if (p === "/api/mock/plan-review/reset" && method === "POST") {
    reset();
    json(res, 200, {});
    return true;
  }
  if ((mm = p.match(/^\/api\/harness\/sessions\/([^/]+)\/goals(?:\/([a-z]+))?$/))) {
    const m = members.get(decodeURIComponent(mm[1]!));
    if (!m) return false;
    if (mm[2] === "start" && method === "POST" && m.stack) {
      m.stack.state = "running";
      m.stack.reason = undefined;
      m.stack.held_for = undefined;
      m.info.held = undefined;
      tell(m);
    }
    json(res, 200, stackView(m.info.id, m));
    return true;
  }
  if ((mm = p.match(/^\/api\/harness\/sessions\/([^/]+)\/command$/)) && method === "POST") {
    const m = members.get(decodeURIComponent(mm[1]!));
    if (!m) return false;
    answerProposal(m, await body(req));
    json(res, 200, {});
    return true;
  }
  if (p === "/api/plans" && method === "GET") {
    const session = url.searchParams.get("session");
    if (!session || !members.has(session)) return false;
    const proposed = url.searchParams.get("proposed") === "1";
    const mine = [...plans.values()].map((k) => k.plan).filter((pl) => pl.source?.session === session && (!proposed || pl.source?.proposed));
    json(
      res,
      200,
      mine.map(({ reviewed: _r, ...pl }) => pl),
    );
    return true;
  }
  if ((mm = p.match(/^\/api\/plans\/([^/]+)(\/versions|\/review|\/runs|\/run)?$/))) {
    const kept = plans.get(decodeURIComponent(mm[1]!));
    if (!kept) return false;
    const goal = kept.plan.source?.goal;
    const m = goal ? members.get(goal.agent) : undefined;
    if (mm[2] === "/run") {
      // Run while its goals wait on its review: approved instead, its steps going as the goal's parts.
      if (method !== "POST" || !goal || m?.stack?.held_for !== kept.plan.id) return false;
      const [status, out] = review(kept.plan.id, { answer: "approve", key: (await body<{ key?: string }>(req)).key });
      const sent = m.stack.items.filter((i) => i.part_of === goal.id && i.part_kind === "plan" && (i.status === "pending" || i.status === "active")).length;
      json(res, status, status === 200 ? { approved: true, session: goal.agent, sent, ...(out as object) } : out);
    } else if (mm[2] === "/review" && method === "POST") {
      const [status, out] = review(kept.plan.id, await body(req));
      json(res, status, out);
    } else if (mm[2] === "/versions") json(res, 200, kept.versions);
    else if (mm[2] === "/runs") json(res, 200, []);
    else json(res, 200, kept.plan);
    return true;
  }
  if (p === "/api/research" && method === "GET") {
    const agent = url.searchParams.get("agent");
    const plan = url.searchParams.get("plan");
    if (!(agent && members.has(agent)) && !(plan && plans.has(plan))) return false;
    json(res, 200, { research: [...RESEARCH.values()].filter((r) => (!agent || r.agent === agent) && (!plan || r.plan === plan)) });
    return true;
  }
  if ((mm = p.match(/^\/api\/research\/([^/]+)$/)) && method === "GET") {
    const r = RESEARCH.get(decodeURIComponent(mm[1]!));
    if (!r) return false;
    json(res, 200, r);
    return true;
  }
  return false;
}
