import assert from "node:assert/strict";
import { test } from "node:test";
import type { ShipCommit } from "../src/api/ship.ts";
import { fold, keyOf, order, pickCard, shipGroups, standWord, strip } from "../src/views/ship/model.ts";

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

test("w26_r14: Ship's stages as the Lanes' groups, a lane for each Checks leg that ran and each deploy", () => {
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
  assert.deepEqual(groups[1]?.lanes, [{ id: "ci/lint", name: "lint", state: "passed", start: 100, end: 160 }]);
  assert.equal(groups[1]?.state, "passed");
  assert.deepEqual(groups[3]?.lanes, [{ id: "staging@200", name: "staging", state: "passed", start: 200, end: 200, note: "synced" }]);
  assert.equal(groups[3]?.state, "soaking");
  assert.deepEqual(groups[4]?.lanes, []);
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
