import assert from "node:assert/strict";
import { test } from "node:test";
import type { ShipCommit } from "../src/api/ship.ts";
import {
  buildWord,
  environmentsOf,
  failuresOf,
  fold,
  keptBuildsOf,
  keyOf,
  order,
  pickCard,
  replantGlyph,
  replantWord,
  ridesOf,
  shipGroups,
  standWord,
  strip,
  traceOf,
} from "../src/views/ship/model.ts";

const c = (sha: string, stand: string, extra: Partial<ShipCommit> = {}): ShipCommit => ({
  repo: "r",
  sha,
  short: sha.slice(0, 7),
  message: sha,
  author: "ada",
  branch: "main",
  at: 0,
  stand,
  line: "",
  pips: [],
  cursor: 0,
  ...extra,
});

test("w26_r01: a change's other commits fold under its tip, and stand alone when the tip isn't on the page", () => {
  const items = fold([c("tip", "waiting"), c("a", "waiting", { fold_under: "tip" }), c("b", "waiting", { fold_under: "gone" })]);
  assert.deepEqual(
    items.map((i) => [i.head.sha, i.riders.map((r) => r.sha)]),
    [
      ["tip", ["a"]],
      ["b", []],
    ],
  );
});

test("w26_r02: needs you first, then failing, by stand; newest keeps Ship's order", () => {
  const items = fold([c("1", "shipped"), c("2", "waiting"), c("3", "needs_you"), c("4", "failing"), c("5", "needs_you")]);
  assert.deepEqual(
    order(items, "needs").map((i) => i.head.sha),
    ["3", "5", "4", "2", "1"],
  );
  assert.deepEqual(
    order(items, "newest").map((i) => i.head.sha),
    ["1", "2", "3", "4", "5"],
  );
  assert.equal(standWord("needs_you"), "Needs you");
  assert.equal(standWord("superseded"), "Superseded");
});

test("w26_r02: the strip draws every pip the server sends, Review first; a state with no glyph yet shows as not yet", () => {
  const pips = [
    { stage: "review", state: "requested" },
    { stage: "land", state: "none" },
    { stage: "checks", state: "passed" },
    { stage: "build", state: "levitating" },
  ];
  assert.deepEqual(
    strip(pips).map((p) => [p.stage, p.state]),
    [
      ["review", "requested"],
      ["land", "none"],
      ["checks", "passed"],
      ["build", "none"],
    ],
  );
});

test("the job card picked first: the running one, else the one that failed, else the last that ran", () => {
  const cards = (...states: string[]) => states.map((state) => ({ state }));
  assert.equal(pickCard(cards("passed", "running", "queued", "none", "none", "none")), 1);
  assert.equal(pickCard(cards("passed", "passed", "soaking", "none", "none", "none")), 2);
  assert.equal(pickCard(cards("passed", "failed", "superseded", "none", "none", "none")), 1);
  assert.equal(pickCard(cards("passed", "passed", "passed", "healthy", "passed", "replanted")), 5);
  assert.equal(pickCard(cards("passed", "passed", "passed", "passed", "none", "none")), 3);
  assert.equal(pickCard(cards("queued", "none", "none", "none", "none", "none")), 0);
});

test("w26_r14: Ship's stages as the Lanes' groups, every job its lane from the start, and each deploy", () => {
  const detail = {
    ...c("a1", "in_staging"),
    pips: [
      { stage: "checks", state: "passed" },
      { stage: "staging", state: "soaking" },
    ],
    checks: [
      {
        pipeline: "ci",
        legs: [
          { name: "lint", state: "passed", attempts: 1, note: "", job: "j1", run: "r1", start: 100, end: 160 },
          { name: "test", state: "queued", attempts: 1, note: "" },
        ],
      },
    ],
    deployments: [{ environment: "staging", production: false, state: "success", description: "synced", at: 200 }],
  };
  const groups = shipGroups(detail);
  assert.deepEqual(
    groups.map((g) => g.group),
    ["land", "checks", "build", "staging", "production", "replant"],
  );
  // Every leg its lane from the start: the queued one too, with no bar until it has times.
  assert.deepEqual(groups[1]?.lanes, [
    { id: "ci/lint", name: "lint", state: "passed", start: 100, end: 160 },
    { id: "ci/test", name: "test", state: "queued", start: null, end: null },
  ]);
  assert.equal(groups[1]?.state, "passed");
  assert.deepEqual(groups[3]?.lanes, [{ id: "staging@200", name: "staging", state: "passed", start: 200, end: 200, note: "synced" }]);
  assert.equal(groups[3]?.state, "soaking");
  assert.deepEqual(groups[4]?.lanes, []);
  // Each platform and the image, and each runner, its lane from the start.
  const later = shipGroups({
    ...detail,
    build: { platforms: [{ name: "linux/amd64", state: "running" }], image: { state: "queued", digest: null } },
    replant: [{ name: "adacachy", state: "replanting", why: "a newer kiln" }],
  });
  assert.deepEqual(later[2]?.lanes, [
    { id: "platform/linux/amd64", name: "linux/amd64", state: "running", start: null, end: null },
    { id: "image", name: "image", state: "queued", start: null, end: null },
  ]);
  assert.deepEqual(later[5]?.lanes, [{ id: "runner/adacachy", name: "adacachy", state: "replanting", start: null, end: null, note: "a newer kiln" }]);
});

test("an open row never folds: not under the approval it rides with, nor into a run of shipped commits", () => {
  const approve = [{ name: "approve", method: "POST", path: "/api/deploy/releases/r1/approve", body: {} }];
  const commits = [c("ask", "needs_you", { actions: approve }), c("rider", "waiting"), c("s1", "shipped"), c("s2", "shipped")];
  const rows = (open: ReadonlySet<string>) => fold(commits, open).map((i) => [i.head.sha, i.riders.map((r) => r.sha)]);
  // Closed, the rider rides with the approval and the older shipped commit folds under the newer.
  assert.deepEqual(rows(new Set()), [
    ["ask", ["rider"]],
    ["s1", ["s2"]],
  ]);
  // Open, each keeps its own row.
  const open = new Set(
    fold(commits)
      .flatMap((i) => [i.head, ...i.riders])
      .map((h) => keyOf({ head: h, riders: [] })),
  );
  assert.deepEqual(rows(new Set(["r/rider", "r/s2"])), [
    ["ask", []],
    ["rider", []],
    ["s1", []],
    ["s2", []],
  ]);
  assert.ok(open.has("r/rider") && open.has("r/s2"));
});

test("the commit page's Errors: every failing leg with its log, then each platform, deploy and runner that failed", () => {
  const detail = {
    ...c("f1", "needs_you"),
    checks: [
      {
        pipeline: "ci",
        legs: [
          { name: "lint", state: "passed", attempts: 1, note: "" },
          { name: "rust lint + test", state: "failed", attempts: 2, note: "assertion failed", job: "j9", run: "r9" },
          { name: "flaky", state: "error", attempts: 1, note: "runner lost", run: "r9" },
        ],
      },
    ],
    build: {
      platforms: [
        { name: "linux/amd64", state: "failed" as const },
        { name: "macos/arm64", state: "passed" as const },
      ],
      image: { state: "queued" as const, digest: null },
    },
    deployments: [{ environment: "staging", production: false, state: "failure", description: "health check failed", at: 300 }],
    replant: [{ name: "adacachy", state: "failed", why: "upgrade refused" }],
  };
  assert.deepEqual(failuresOf(detail), [
    { stage: "checks", name: "rust lint + test", note: "assertion failed", href: "/runs/r9/jobs/j9" },
    { stage: "checks", name: "flaky", note: "runner lost", href: "/runs/r9" },
    { stage: "build", name: "linux/amd64", note: "", href: null },
    { stage: "staging", name: "staging", note: "health check failed", href: null },
    { stage: "replant", name: "adacachy", note: "upgrade refused", href: null },
  ]);
  assert.deepEqual(failuresOf(c("ok", "shipped")), []);
});

test("the commit page's trace: every lane of every group, failures first, then by when it started", () => {
  const spans = traceOf([
    { group: "review", label: "Review", state: "passed", note: "", lanes: [{ id: "m1", name: "v1 · pushed", state: "passed", start: 10, end: 10 }] },
    {
      group: "checks",
      label: "Checks",
      state: "failed",
      note: "",
      lanes: [
        { id: "ci/lint", name: "lint", state: "passed", start: 20, end: 30 },
        { id: "ci/test", name: "test", state: "failed", start: 25, end: 40 },
        { id: "ci/later", name: "later", state: "queued", start: null, end: null },
      ],
    },
  ]);
  assert.deepEqual(
    spans.map((x) => x.name),
    ["Checks · test", "Review · v1 · pushed", "Checks · lint", "Checks · later"],
  );
  assert.equal(spans[0]?.id, "checks/ci/test");
});

test("what ships with a commit: what folds under it, what it rides with, or nothing", () => {
  const commits = [c("tip", "needs_you", { change: { name: "D1" } as ShipCommit["change"] }), c("old", "superseded", { fold_under: "tip" }), c("lone", "shipped")];
  assert.deepEqual(ridesOf(commits, "tip").as, "head");
  assert.deepEqual(
    ridesOf(commits, "tip").with.map((r) => r.sha),
    ["old"],
  );
  const rider = ridesOf(commits, "old");
  assert.equal(rider.as, "rider");
  assert.equal(rider.head?.sha, "tip");
  assert.deepEqual(rider.with, []);
  assert.equal(ridesOf(commits, "lone").as, "alone");
  assert.equal(ridesOf(commits, "unknown").as, "alone");
});

test("where each environment stands from a commit: its latest deploy there, and whether it runs it now", () => {
  const envs = environmentsOf({
    ...c("s", "shipped"),
    environments: [{ environment: "production", production: true }],
    deployments: [
      { environment: "production", production: true, state: "success", description: "promoted", at: 300 },
      { environment: "staging", production: false, state: "failure", description: "first try", at: 100 },
      { environment: "staging", production: false, state: "success", description: "synced", at: 200 },
    ],
  });
  assert.deepEqual(envs, [
    { environment: "staging", production: false, state: "passed", description: "synced", at: 200, now: false },
    { environment: "production", production: true, state: "passed", description: "promoted", at: 300, now: true },
  ]);
});

test("a commit's Build: each platform's release build in words, a newer push's superseding said so", () => {
  assert.deepEqual(["passed", "running", "queued", "failed", "superseded"].map(buildWord), ["built", "building", "queued", "failed", "superseded by a newer push"]);
});

test("from System's builds: the platforms whose runners get a commit's binary, and those whose binary from it a newer build replaced", () => {
  const platforms = [
    { label: "linux · x86_64 · gnu", target: "x86_64-unknown-linux-gnu", binary: { sha: "aaaa1111" }, shadowed: [{ sha: "bbbb2222" }] },
    { label: "linux · aarch64 · gnu", target: "aarch64-unknown-linux-gnu", binary: { sha: "bbbb2222" }, shadowed: [] },
    { label: "macos · arm64", target: "aarch64-apple-darwin", binary: null, shadowed: [] },
  ];
  assert.deepEqual(keptBuildsOf(platforms, "bbbb2222cccc"), [
    { label: "linux · x86_64 · gnu", target: "x86_64-unknown-linux-gnu", state: "replaced" },
    { label: "linux · aarch64 · gnu", target: "aarch64-unknown-linux-gnu", state: "current" },
  ]);
  assert.deepEqual(keptBuildsOf(platforms, "aaaa1111"), [{ label: "linux · x86_64 · gnu", target: "x86_64-unknown-linux-gnu", state: "current" }]);
  assert.deepEqual(keptBuildsOf(platforms, "ffff0000"), []);
});

test("a runner's replant, as the read model says it, in glyphs and words, and in the Lanes", () => {
  const states = ["on", "waiting", "failed", "offline"];
  assert.deepEqual(states.map(replantGlyph), ["replanted", "replanting", "failed", "offline"]);
  assert.deepEqual(states.map(replantWord), ["replanted", "replanting", "didn't replant", "offline"]);
  const lanes = shipGroups({ ...c("r", "shipped"), replant: [{ name: "pi", state: "failed", why: "timed out" }] })[5]?.lanes;
  assert.deepEqual(lanes, [{ id: "runner/pi", name: "pi", state: "failed", start: null, end: null, note: "timed out" }]);
});
