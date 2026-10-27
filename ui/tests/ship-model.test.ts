import assert from "node:assert/strict";
import { test } from "node:test";
import type { ShipCommit } from "../src/api/ship.ts";
import { fold, order, reviewWord, standWord, strip } from "../src/views/ship/model.ts";

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

test("w26_r02: the strip draws Review first, then the stages after it; what it doesn't know waits for its glyph", () => {
  const pips = [
    { stage: "review", state: "requested" },
    { stage: "land", state: "none" },
    { stage: "checks", state: "passed" },
    { stage: "orbit", state: "passed" },
    { stage: "build", state: "levitating" },
  ];
  assert.deepEqual(
    strip(pips).map((p) => [p.stage, p.state]),
    [
      ["review", "requested"],
      ["land", "none"],
      ["checks", "passed"],
    ],
  );
  const d41 = c("d", "needs_you", { pips, change: { name: "D41", kind: "diff", host: "kiln", title: "t", revision: 1, latest: 1, state: "requested", turn: [], open: null, superseded_by: null } });
  assert.equal(reviewWord(d41), "D41 · requested");
  assert.equal(reviewWord(c("x", "shipped")), null);
});
