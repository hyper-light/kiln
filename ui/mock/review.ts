/**
 * The mock's review (docs/ci.md W14, the API of crates/kiln/src/ci/diffs.rs and review.rs): a
 * stack of three diffs and a change request an agent opened, with a patch that has everything the
 * review page draws (word changes, a block moved to a new file, whitespace-only changes, a rename
 * with edits, a generated lockfile, an image, a deleted script, docs), threads (open, a draft of
 * yours, resolved, outdated), checks, landing, the inbox and the land queue; and D43, ten thousand
 * changed lines, for scrolling. What's sent changes it, as the server would.
 */
import type http from "node:http";

interface Ctx {
  json: (res: http.ServerResponse, status: number, body: unknown) => void;
  err: (res: http.ServerResponse, status: number, message: string) => void;
  body: <T>(req: http.IncomingMessage) => Promise<T>;
  emit: (ev: { type: string; [k: string]: unknown }) => void;
}

const REPO = "hyper-light/kiln";
const ME = "user:ada";
const now = () => Math.floor(Date.now() / 1000);
const T0 = now() - 3 * 86400;

interface MockDiff {
  id: string;
  name: string;
  number: number;
  kind: "diff" | "change_request";
  repo: string;
  change_id: string | null;
  title: string;
  summary: string;
  author: string;
  author_kind: string;
  author_for: string | null;
  state: string;
  trunk: string;
  stack: string | null;
  parent: string | null;
  topic: string | null;
  created_at: number;
  updated_at: number;
  branch: string | null;
  versions: number;
}

const diffs: MockDiff[] = [
  {
    id: "d40",
    name: "D40",
    number: 40,
    kind: "diff",
    repo: REPO,
    change_id: "I40",
    title: "pipeline: job outputs",
    summary: "Jobs say what they made: `outputs:` read by the jobs that need them.",
    author: "user:sam",
    author_kind: "user",
    author_for: null,
    state: "accepted",
    trunk: "main",
    stack: "s1",
    parent: null,
    topic: "matrices",
    created_at: T0,
    updated_at: T0 + 7200,
    branch: null,
    versions: 1,
  },
  {
    id: "d41",
    name: "D41",
    number: 41,
    kind: "diff",
    repo: REPO,
    change_id: "I41",
    title: "pipeline: dynamic matrices",
    summary:
      "A matrix may come from a job's output (`fromJSON(needs.plan.outputs.legs)`).\n\n- `expand` moves to its own module, as include and exclude are coming.\n- Plans say which legs a matrix made, and why one was left out.",
    author: "user:sam",
    author_kind: "user",
    author_for: null,
    state: "needs-review",
    trunk: "main",
    stack: "s1",
    parent: "D40",
    topic: "matrices",
    created_at: T0 + 3600,
    updated_at: now() - 1800,
    branch: null,
    versions: 2,
  },
  {
    id: "d42",
    name: "D42",
    number: 42,
    kind: "diff",
    repo: REPO,
    change_id: "I42",
    title: "pipeline: matrix include and exclude",
    summary: "",
    author: ME,
    author_kind: "user",
    author_for: null,
    state: "draft",
    trunk: "main",
    stack: "s1",
    parent: "D41",
    topic: "matrices",
    created_at: T0 + 7200,
    updated_at: now() - 600,
    branch: null,
    versions: 1,
  },
  {
    id: "cr7",
    name: "CR7",
    number: 7,
    kind: "change_request",
    repo: REPO,
    change_id: null,
    title: "Retry backoff rework",
    summary: "The retry loop read the clock twice, so a slow tick could put the deadline before the first attempt.\n\nAll 160 tests pass, ten runs in a row.",
    author: "agent:5f3a2b1c",
    author_kind: "agent",
    author_for: ME,
    state: "needs-review",
    trunk: "main",
    stack: null,
    parent: null,
    topic: null,
    created_at: now() - 5400,
    updated_at: now() - 900,
    branch: "agents/retry-storm",
    versions: 1,
  },
  {
    id: "d43",
    name: "D43",
    number: 43,
    kind: "diff",
    repo: REPO,
    change_id: "I43",
    title: "Regenerate the expression test vectors (10,000 lines)",
    summary: "",
    author: "user:sam",
    author_kind: "user",
    author_for: null,
    state: "needs-review",
    trunk: "main",
    stack: null,
    parent: null,
    topic: null,
    created_at: now() - 7200,
    updated_at: now() - 7200,
    branch: null,
    versions: 1,
  },
];

const sha = (s: string) => {
  let h = 2166136261;
  for (const c of s) h = Math.imul(h ^ c.charCodeAt(0), 16777619) >>> 0;
  return (h.toString(16) + "0".repeat(40)).slice(0, 8) + "c0ffee" + h.toString(16).padStart(8, "0") + "deadbeefcafe0011";
};

/* ------------------------------------------------------------------ the patch */

const EXPAND = [
  "/// The legs a matrix makes: every combination of its axes, in order.",
  "pub fn expand(axes: &[(String, Vec<Value>)]) -> Vec<BTreeMap<String, Value>> {",
  "    let mut legs = vec![BTreeMap::new()];",
  "    for (name, values) in axes {",
  "        legs = legs",
  "            .into_iter()",
  "            .flat_map(|leg| values.iter().map(move |v| with(&leg, name, v)))",
  "            .collect();",
  "    }",
  "    legs",
  "}",
];

function d41Patch(v: number): string {
  const later = v >= 2;
  const lines: string[] = [];
  const push = (...l: string[]) => lines.push(...l);
  // A modified file: word changes, and expand() moved out.
  push(
    "diff --git a/crates/kiln-ci/src/plan/matrix.rs b/crates/kiln-ci/src/plan/matrix.rs",
    "index 3b18e1c..9f2a7d0 100644",
    "--- a/crates/kiln-ci/src/plan/matrix.rs",
    "+++ b/crates/kiln-ci/src/plan/matrix.rs",
    "@@ -1,9 +1,10 @@",
    " //! Matrices: a job run once per combination of its axes (docs/ci.md W04-R03).",
    " ",
    " use std::collections::BTreeMap;",
    " ",
    "-use serde_json::Value;",
    "+use serde_json::{Value, json};",
    "+",
    "+use super::expand::expand;",
    " ",
    "-/// A matrix as written: its axes, each a name and its values.",
    "+/// A matrix as written: its axes, each a name and its values, or an expression that makes them.",
    " pub struct Matrix {",
    `@@ -14,21 +15,13 @@ impl Matrix {`,
    "     pub fn legs(&self, ctx: &Context) -> Result<Vec<Leg>, Error> {",
    "-        let axes = self.axes.clone();",
    later ? "+        let axes = self.resolve(ctx)?;" : "+        let axes = self.resolve(ctx).unwrap_or_default();",
    "         Ok(expand(&axes).into_iter().map(Leg::from).collect())",
    "     }",
    " }",
    " ",
    ...EXPAND.map((l) => `-${l}`),
    "-",
    " fn with(leg: &BTreeMap<String, Value>, name: &str, v: &Value) -> BTreeMap<String, Value> {",
    "     let mut next = leg.clone();",
    "     next.insert(name.to_owned(), v.clone());",
  );
  // The new file the block moved to.
  push(
    "diff --git a/crates/kiln-ci/src/plan/expand.rs b/crates/kiln-ci/src/plan/expand.rs",
    "new file mode 100644",
    "index 0000000..4e5f6a7",
    "--- /dev/null",
    "+++ b/crates/kiln-ci/src/plan/expand.rs",
    `@@ -0,0 +1,${EXPAND.length + 5} @@`,
    "+//! Expanding a matrix's axes into its legs (docs/ci.md W04-R03).",
    "+",
    "+use std::collections::BTreeMap;",
    "+use serde_json::Value;",
    "+",
    ...EXPAND.map((l) => `+${l}`),
  );
  // Whitespace only (and one real change).
  push(
    "diff --git a/crates/kiln-ci/src/plan/mod.rs b/crates/kiln-ci/src/plan/mod.rs",
    "index 51a2b3c..62c3d4e 100644",
    "--- a/crates/kiln-ci/src/plan/mod.rs",
    "+++ b/crates/kiln-ci/src/plan/mod.rs",
    "@@ -3,8 +3,9 @@",
    " pub mod context;",
    "+pub mod expand;",
    " pub mod matrix;",
    "-pub mod  reuse;",
    "-    pub use matrix::Matrix;",
    "+pub mod reuse;",
    "+pub use matrix::Matrix;",
    " pub use reuse::Reuse;",
    " ",
    " /// A plan: the jobs a run will have, and why each is there.",
  );
  // Docs.
  push(
    "diff --git a/docs/pipelines.md b/docs/pipelines.md",
    "index 7a8b9c0..8b9c0d1 100644",
    "--- a/docs/pipelines.md",
    "+++ b/docs/pipelines.md",
    "@@ -40,6 +40,13 @@ A job runs once per leg of its matrix.",
    " ```yaml",
    " strategy:",
    "   matrix:",
    "     os: [linux, macos]",
    " ```",
    "+",
    "+## A matrix made by a job",
    "+",
    "+A matrix may come from what an earlier job said (`outputs:`). The plan shows each leg it made,",
    "+and why one was left out: **a matrix's legs are known once the job before it ends**.",
    "+See [outputs](#outputs).",
    " ",
  );
  // A rename with edits.
  push(
    "diff --git a/ui/src/views/run-detail/dag.tsx b/ui/src/views/run-detail/graph.tsx",
    "similarity index 92%",
    "rename from ui/src/views/run-detail/dag.tsx",
    "rename to ui/src/views/run-detail/graph.tsx",
    "index 1c2d3e4..2d3e4f5 100644",
    "--- a/ui/src/views/run-detail/dag.tsx",
    "+++ b/ui/src/views/run-detail/graph.tsx",
    '@@ -12,7 +12,7 @@ import { StatusDot } from "@/components/ui/status";',
    " ",
    " /** The run's jobs as a graph: each job a node, its needs the edges. */",
    "-export function Dag({ run }: { run: RunDetail }) {",
    "+export function Graph({ run, legs }: { run: RunDetail; legs: Leg[] }) {",
    "   const nodes = React.useMemo(() => layout(run.jobs), [run.jobs]);",
    "   return (",
    '     <svg className="dag" viewBox={`0 0 ${nodes.width} ${nodes.height}`}>',
  );
  // Generated.
  push(
    "diff --git a/Cargo.lock b/Cargo.lock",
    "index aaa1111..bbb2222 100644",
    "--- a/Cargo.lock",
    "+++ b/Cargo.lock",
    "@@ -1201,7 +1201,7 @@ dependencies = [",
    " [[package]]",
    ' name = "serde_json"',
    '-version = "1.0.140"',
    '+version = "1.0.141"',
    ' source = "registry+https://github.com/rust-lang/crates.io-index"',
  );
  // An image.
  push("diff --git a/docs/assets/matrix.png b/docs/assets/matrix.png", "new file mode 100644", "index 0000000..c4d5e6f", "Binary files /dev/null and b/docs/assets/matrix.png differ");
  // A deleted script.
  push(
    "diff --git a/scripts/expand-matrix.sh b/scripts/expand-matrix.sh",
    "deleted file mode 100755",
    "index 9e8d7c6..0000000",
    "--- a/scripts/expand-matrix.sh",
    "+++ /dev/null",
    "@@ -1,4 +0,0 @@",
    "-#!/bin/sh",
    "-# Superseded by plan::expand.",
    "-set -eu",
    '-exec cargo run -q -p kiln-ci --bin expand -- "$@"',
  );
  return lines.join("\n") + "\n";
}

function cr7Patch(): string {
  return (
    [
      "diff --git a/crates/kiln/src/http/retry.rs b/crates/kiln/src/http/retry.rs",
      "index 0a1b2c3..1b2c3d4 100644",
      "--- a/crates/kiln/src/http/retry.rs",
      "+++ b/crates/kiln/src/http/retry.rs",
      "@@ -20,10 +20,10 @@ impl Backoff {",
      "     pub fn next(&mut self, rng: &mut impl Rng) -> Option<Duration> {",
      "         self.attempt += 1;",
      "-        let deadline = Instant::now() + self.budget;",
      "-        if Instant::now() >= deadline {",
      "+        let start = self.started.get_or_insert_with(Instant::now);",
      "+        if start.elapsed() >= self.budget {",
      "             return None;",
      "         }",
      "-        let jitter = rand::random::<f64>() * self.base.as_secs_f64();",
      "+        let jitter = rng.random::<f64>() * self.base.as_secs_f64();",
      "         Some(self.base * 2u32.saturating_pow(self.attempt) + Duration::from_secs_f64(jitter))",
      "     }",
      " }",
    ].join("\n") + "\n"
  );
}

/** D43: ten thousand changed lines (half changed in place, half added), to scroll through. */
function bigPatch(): string {
  const out = [
    "diff --git a/crates/kiln-ci/tests/vectors.rs b/crates/kiln-ci/tests/vectors.rs",
    "index 1111111..2222222 100644",
    "--- a/crates/kiln-ci/tests/vectors.rs",
    "+++ b/crates/kiln-ci/tests/vectors.rs",
  ];
  const n = 2500;
  out.push(`@@ -1,${n * 2} +1,${n * 3} @@`);
  for (let i = 0; i < n; i++) {
    out.push(`-    vector!(${i}, "contains(github.event.pull_request.labels.*.name, 'v${i}')", false);`);
    out.push(`+    vector!(${i}, "contains(github.event.pull_request.labels.*.name, 'v${i}')", true);`);
    out.push(`+    vector!(${i + n}, "startsWith(github.ref, 'refs/tags/v${i}')", ${i % 2 === 0});`);
    out.push(`     // ${i}: see docs/ci.md Appendix E`);
  }
  return out.join("\n") + "\n";
}

function patchOf(d: MockDiff, v: number): string {
  if (d.name === "D41") return d41Patch(v);
  if (d.name === "CR7") return cr7Patch();
  if (d.name === "D43") return bigPatch();
  return (
    [
      `diff --git a/crates/kiln-ci/src/plan/${d.name.toLowerCase()}.rs b/crates/kiln-ci/src/plan/${d.name.toLowerCase()}.rs`,
      "new file mode 100644",
      "--- /dev/null",
      `+++ b/crates/kiln-ci/src/plan/${d.name.toLowerCase()}.rs`,
      "@@ -0,0 +1,3 @@",
      `+//! ${d.title}`,
      "+",
      "+pub fn todo() {}",
    ].join("\n") + "\n"
  );
}

/** The files of a patch as numstat and name-status say them. */
function filesOf(patch: string): { path: string; old_path: string | null; added: number; deleted: number; binary: boolean; status: string; generated?: boolean }[] {
  const out: ReturnType<typeof filesOf> = [];
  let cur: (typeof out)[number] | null = null;
  for (const l of patch.split("\n")) {
    if (l.startsWith("diff --git ")) {
      const m = l.match(/^diff --git a\/(.+) b\/(.+)$/);
      cur = { path: m?.[2] ?? "", old_path: null, added: 0, deleted: 0, binary: false, status: "M" };
      if (cur.path === "Cargo.lock") cur.generated = true;
      out.push(cur);
    } else if (!cur) continue;
    else if (l.startsWith("new file mode")) cur.status = "A";
    else if (l.startsWith("deleted file mode")) cur.status = "D";
    else if (l.startsWith("rename from ")) {
      cur.old_path = l.slice(12);
      cur.status = "R092";
    } else if (l.startsWith("Binary files")) cur.binary = true;
    else if (l.startsWith("+") && !l.startsWith("+++")) cur.added++;
    else if (l.startsWith("-") && !l.startsWith("---")) cur.deleted++;
  }
  return out;
}

/* ------------------------------------------------------------------ threads */

interface MockComment {
  id: string;
  author: string;
  body: string;
  suggestion: string | null;
  created_at: number;
  published: boolean;
}
interface MockThread {
  id: string;
  diff: string;
  version: number;
  file: string | null;
  side: "old" | "new";
  start_line: number | null;
  end_line: number | null;
  state: string;
  created_by: string;
  created_at: number;
  comments: MockComment[];
}

let seq = 100;
const id = (p: string) => `${p}${++seq}`;
const threads: MockThread[] = [
  {
    id: "t1",
    diff: "D41",
    version: 2,
    file: "crates/kiln-ci/src/plan/matrix.rs",
    side: "new",
    start_line: 16,
    end_line: 16,
    state: "open",
    created_by: "user:kim",
    created_at: now() - 3000,
    comments: [
      {
        id: "c1",
        author: "user:kim",
        body: "`resolve` can fail on a bad expression: say which leg's axis it was, or the plan's error reads as if the whole matrix broke.",
        suggestion: null,
        created_at: now() - 3000,
        published: true,
      },
      { id: "c2", author: "user:sam", body: "It names the axis already; I'll add the job too.", suggestion: null, created_at: now() - 2400, published: true },
    ],
  },
  {
    id: "t2",
    diff: "D41",
    version: 2,
    file: "crates/kiln-ci/src/plan/expand.rs",
    side: "new",
    start_line: 7,
    end_line: 9,
    state: "open",
    created_by: ME,
    created_at: now() - 600,
    comments: [{ id: "c3", author: ME, body: "This allocates a map per leg per axis; fine for now, but a 256-leg matrix would notice.", suggestion: null, created_at: now() - 600, published: false }],
  },
  {
    id: "t3",
    diff: "D41",
    version: 1,
    file: "crates/kiln-ci/src/plan/matrix.rs",
    side: "new",
    start_line: 16,
    end_line: 16,
    state: "open",
    created_by: "user:kim",
    created_at: now() - 86400,
    comments: [{ id: "c4", author: "user:kim", body: "`unwrap_or_default` hides a bad matrix as an empty one.", suggestion: null, created_at: now() - 86400, published: true }],
  },
  {
    id: "t4",
    diff: "D41",
    version: 1,
    file: null,
    side: "new",
    start_line: null,
    end_line: null,
    state: "resolved",
    created_by: "user:kim",
    created_at: now() - 90000,
    comments: [{ id: "c5", author: "user:kim", body: "Could this land after D40's outputs? It reads them.", suggestion: null, created_at: now() - 90000, published: true }],
  },
  {
    id: "t5",
    diff: "CR7",
    version: 1,
    file: "crates/kiln/src/http/retry.rs",
    side: "new",
    start_line: 29,
    end_line: 29,
    state: "open",
    created_by: "user:kim",
    created_at: now() - 800,
    comments: [
      {
        id: "c6",
        author: "user:kim",
        body: "Use the backoff's own rng here too, so a seeded test is seeded all the way.",
        suggestion: "        let jitter = rng.random::<f64>() * self.base.as_secs_f64();",
        created_at: now() - 800,
        published: true,
      },
    ],
  },
];

const viewed = new Map<string, Set<string>>();
const queue: {
  id: string;
  trunk: string;
  diffs: string[];
  strategy: string;
  state: string;
  batch: string | null;
  candidate: string | null;
  reason: string | null;
  enqueued_by: string;
  created_at: number;
  updated_at: number;
}[] = [
  {
    id: "q1",
    trunk: "main",
    diffs: ["D39"],
    strategy: "stack",
    state: "testing",
    batch: "b7",
    candidate: "c0ffee1",
    reason: null,
    enqueued_by: "user:sam",
    created_at: now() - 420,
    updated_at: now() - 60,
  },
  {
    id: "q0",
    trunk: "main",
    diffs: ["D37", "D38"],
    strategy: "stack-squash",
    state: "ejected",
    batch: null,
    candidate: null,
    reason: "`ci` failure on the candidate: matrix legs differed from the plan",
    enqueued_by: "user:kim",
    created_at: now() - 7200,
    updated_at: now() - 6000,
  },
];

function view(d: MockDiff) {
  const { versions: n, ...rest } = d;
  const vs = Array.from({ length: n }, (_, i) => ({
    n: i + 1,
    commit: sha(`${d.name}v${i + 1}`),
    parent: sha(`${d.name}p`),
    base: sha("main"),
    rebase_only: false,
    pushed_by: d.author,
    note: i === 1 ? "resolve() fails loudly; the plan says which axis" : null,
    created_at: d.created_at + i * 3600,
    ref: `refs/kiln/diffs/${d.name}/v${i + 1}`,
  }));
  return {
    ...rest,
    versions: vs,
    reviewers:
      d.name === "D41"
        ? [
            { reviewer: "user:kim", state: "changes-requested" },
            { reviewer: ME, state: "not-looked" },
          ]
        : [{ reviewer: ME, state: "not-looked" }],
    runs:
      d.name === "D43"
        ? []
        : [
            { id: "r-ci", number: 812, workflow: "ci", name: "ci", event: "diff", status: "completed", conclusion: d.name === "CR7" ? "success" : "success" },
            { id: "r-bench", number: 813, workflow: "bench", name: "bench", event: "diff", status: d.name === "D41" ? "running" : "completed", conclusion: d.name === "D41" ? null : "success" },
          ],
    ...(d.kind === "change_request"
      ? {
          commits: [
            { sha: sha("cr7c1"), title: "http: one clock read per attempt" },
            { sha: sha("cr7c2"), title: "http: jitter from the backoff's rng" },
          ],
        }
      : {}),
  };
}

function threadsOf(d: MockDiff) {
  const latest = d.versions;
  return threads
    .filter((t) => t.diff === d.name)
    .filter((t) => t.comments.some((c) => c.published || c.author === ME))
    .map((t) => ({
      id: t.id,
      version: t.version,
      file: t.file,
      side: t.side,
      start_line: t.start_line,
      end_line: t.end_line,
      state: t.state,
      created_by: t.created_by,
      created_at: t.created_at,
      outdated: t.id === "t3",
      now: t.file == null ? null : t.id === "t3" ? null : { version: latest, start_line: t.start_line, end_line: t.end_line },
      comments: t.comments
        .filter((c) => c.published || c.author === ME)
        .map((c) => ({ id: c.id, author: c.author, body: c.body, suggestion: c.suggestion, created_at: c.created_at, draft: !c.published })),
    }));
}

function inbox() {
  const row = (t: MockThread) => {
    const d = diffs.find((x) => x.name === t.diff);
    const last = t.comments.filter((c) => c.published).at(-1);
    return { thread: t.id, repo: REPO, diff: t.diff, title: d?.title ?? "", version: t.version, file: t.file, line: t.end_line, last_author: last?.author ?? t.created_by, last: last?.body ?? "" };
  };
  const open = threads.filter((t) => t.state === "open" && t.comments.some((c) => c.published));
  return {
    you: ME,
    waiting_for_you: open.filter((t) => t.comments.filter((c) => c.published).at(-1)?.author !== ME).map(row),
    waiting_for_others: open.filter((t) => t.comments.filter((c) => c.published).at(-1)?.author === ME).map(row),
    review_requested: diffs
      .filter((d) => d.author !== ME && ["needs-review", "changes-requested"].includes(d.state))
      .map((d) => ({ repo: d.repo, diff: d.name, title: d.title, author: d.author, your_state: "not-looked" })),
    your_diffs: diffs
      .filter((d) => d.author === ME || d.author_for === ME)
      .filter((d) => !["landed", "abandoned"].includes(d.state))
      .map((d) => ({ repo: d.repo, diff: d.name, title: d.title, state: d.state })),
  };
}

/** A small PNG (32×32, a check of kiln's grey and green), for the image diff. */
const PNG = Buffer.from("iVBORw0KGgoAAAANSUhEUgAAACAAAAAgCAIAAAD8GO2jAAAAO0lEQVR42mPQ1rPEilac3IMVkaqeYdSCUQuGgAXUMgiX+lELRi0YChaMFhWjFoxaMFofjFowaoG2niUAm6N+TBJRp68AAAAASUVORK5CYII=", "base64");

export async function handleReview(req: http.IncomingMessage, res: http.ServerResponse, url: URL, ctx: Ctx): Promise<boolean> {
  const p = url.pathname;
  const m = req.method ?? "GET";
  const segs = p.split("/").filter(Boolean).slice(1);
  const [a, b, c, d2, e] = segs;
  if (a === "review" && b === "inbox" && m === "GET") return (ctx.json(res, 200, inbox()), true);
  if (a === "stacks" && b && m === "GET") {
    const list = diffs.filter((d) => d.stack === b).map((d) => Object.fromEntries(Object.entries(d).filter(([k]) => k !== "versions")));
    if (list.length === 0) return (ctx.err(res, 404, "no such stack"), true);
    return (ctx.json(res, 200, { id: b, trunk: "main", repo: REPO, diffs: list }), true);
  }
  if (a === "repos" && c === "queue" && m === "GET") return (ctx.json(res, 200, { entries: decodeURIComponent(b ?? "") === REPO ? queue : [] }), true);
  if (a === "queue" && b && m === "DELETE") {
    const q = queue.find((x) => x.id === b);
    if (q) q.state = "cancelled";
    return (ctx.json(res, 200, { ok: true }), true);
  }
  if (a === "comments" && b) {
    const t = threads.find((x) => x.comments.some((cm) => cm.id === b));
    if (!t) return (ctx.err(res, 404, "no such comment"), true);
    if (m === "PATCH") {
      const body = await ctx.body<{ state?: string; body?: string }>(req);
      if (body.state) t.state = body.state === "resolved" ? "resolved" : "open";
      ctx.emit({ type: "comment", diff: t.diff });
      return (ctx.json(res, 200, { id: b, thread: t.id, state: t.state }), true);
    }
    return (ctx.json(res, 200, { ok: true }), true);
  }
  if (a !== "diffs" && a !== "change-requests") return false;
  const d = diffs.find((x) => x.name.toLowerCase() === (b ?? "").toLowerCase() || x.id === b);
  if (!d) return (ctx.err(res, 404, `no diff ${b}`), true);
  if (!c && m === "GET") return (ctx.json(res, 200, view(d)), true);
  if (c === "versions" && d2 && e === "files" && m === "GET") {
    const v = Number(d2);
    const patch = patchOf(d, v);
    const seen = viewed.get(d.name) ?? new Set<string>();
    return (
      ctx.json(res, 200, {
        diff: d.name,
        version: v,
        commit: sha(`${d.name}v${v}`),
        parent: sha(`${d.name}p`),
        files: filesOf(patch).map((f) => ({ ...f, viewed: seen.has(f.path) })),
        patch,
        truncated: false,
      }),
      true
    );
  }
  if (c === "versions" && d2 && e === "blob" && m === "GET") {
    res.writeHead(200, { "content-type": "image/png", "content-length": PNG.length });
    res.end(PNG);
    return true;
  }
  if (c === "interdiff" && m === "GET") {
    const from = Number(url.searchParams.get("from") ?? 1);
    const to = Number(url.searchParams.get("to") ?? d.versions);
    return (
      ctx.json(res, 200, {
        diff: d.name,
        from,
        to,
        same_patch: false,
        range_diff: `1:  ${sha("v1").slice(0, 7)} ! 1:  ${sha("v2").slice(0, 7)} pipeline: dynamic matrices\n    @@ crates/kiln-ci/src/plan/matrix.rs: impl Matrix\n         pub fn legs(&self, ctx: &Context) -> Result<Vec<Leg>, Error> {\n    -        let axes = self.resolve(ctx).unwrap_or_default();\n    +        let axes = self.resolve(ctx)?;\n`,
      }),
      true
    );
  }
  if (c === "comments" && m === "GET") return (ctx.json(res, 200, { threads: threadsOf(d) }), true);
  if (c === "comments" && m === "POST") {
    const body = await ctx.body<{ body?: string; publish?: boolean; thread?: string; version?: number; file?: string; side?: "old" | "new"; start_line?: number; end_line?: number }>(req);
    const cm: MockComment = { id: id("c"), author: ME, body: body.body ?? "", suggestion: null, created_at: now(), published: Boolean(body.publish) };
    let t = body.thread ? threads.find((x) => x.id === body.thread) : undefined;
    if (!t) {
      t = {
        id: id("t"),
        diff: d.name,
        version: body.version ?? d.versions,
        file: body.file ?? null,
        side: body.side ?? "new",
        start_line: body.start_line ?? null,
        end_line: body.end_line ?? body.start_line ?? null,
        state: "open",
        created_by: ME,
        created_at: now(),
        comments: [],
      };
      threads.push(t);
    }
    t.comments.push(cm);
    ctx.emit({ type: "comment", diff: d.name });
    return (ctx.json(res, 200, { id: cm.id, thread: t.id, draft: !cm.published }), true);
  }
  if (c === "review" && m === "POST") {
    const body = await ctx.body<{ action?: string; body?: string }>(req);
    let published = 0;
    for (const t of threads.filter((x) => x.diff === d.name))
      for (const cm of t.comments)
        if (!cm.published && cm.author === ME) {
          cm.published = true;
          published++;
        }
    if (body.action === "accept") d.state = "accepted";
    else if (body.action === "request_changes") d.state = "changes-requested";
    d.updated_at = now();
    ctx.emit({ type: "review", diff: d.name });
    return (ctx.json(res, 200, { diff: d.name, action: body.action, version: d.versions, published, state: d.state }), true);
  }
  if (c === "viewed" && m === "POST") {
    const body = await ctx.body<{ file: string; viewed?: boolean }>(req);
    const set = viewed.get(d.name) ?? new Set<string>();
    if (body.viewed === false) set.delete(body.file);
    else set.add(body.file);
    viewed.set(d.name, set);
    return (ctx.json(res, 200, { file: body.file, viewed: body.viewed !== false, version: d.versions }), true);
  }
  if (c === "land" && m === "GET") {
    const blockers =
      d.state === "accepted"
        ? []
        : d.name === "D41"
          ? ["Needs 1 accept from someone who isn't its author (kim requested changes)", "`bench` hasn't finished on this version"]
          : ["Needs 1 accept from someone who isn't its author"];
    return (
      ctx.json(res, 200, {
        blockers,
        accepting: [],
        requesting: [],
        checks: { ci: "success", bench: d.name === "D41" ? null : "success" },
        required: ["ci"],
        strategies: d.stack ? ["stack", "stack-squash"] : ["squash", "merge", "rebase"],
        default: d.stack ? "stack" : "squash",
      }),
      true
    );
  }
  if (c === "land" && m === "POST") {
    if (d.state !== "accepted") return (ctx.err(res, 409, `${d.name} needs an accept first`), true);
    d.state = "landing";
    queue.unshift({
      id: id("q"),
      trunk: "main",
      diffs: [d.name],
      strategy: "stack",
      state: "queued",
      batch: null,
      candidate: null,
      reason: null,
      enqueued_by: ME,
      created_at: now(),
      updated_at: now(),
    });
    ctx.emit({ type: "queue" });
    return (ctx.json(res, 200, { ok: true }), true);
  }
  if (["ready", "abandon", "reclaim", "commandeer"].includes(c ?? "") && m === "POST") {
    d.state = c === "ready" ? "needs-review" : c === "abandon" ? "abandoned" : c === "reclaim" ? "needs-review" : d.state;
    ctx.emit({ type: "diff", diff: d.name });
    return (ctx.json(res, 200, { ok: true, state: d.state }), true);
  }
  return false;
}
