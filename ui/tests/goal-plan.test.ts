import assert from "node:assert/strict";
import { test } from "node:test";
import { goalPlanRows, machineWord, shortTitle, type GoalPlanRow, type GoalStep } from "../src/views/harness/goal-plan-layout.ts";

const step = (n: number, title: string, needs: number[], stand: GoalStep["stand"] = "waiting"): GoalStep => ({ n, title, needs, stand });

/** Rows as words: "1", "fork 2", "b2", "join", "folded 4", "10 after 1". */
const said = (rows: GoalPlanRow[]) =>
  rows.map((r) =>
    r.kind === "step"
      ? `${r.lane === "branch" ? "b" : ""}${r.step.n}${r.after.length ? ` after ${r.after.map((a) => a.n).join(",")}` : ""}${r.start ? " from the start" : ""}`
      : r.kind === "fork" || r.kind === "folded"
        ? `${r.kind} ${r.count}`
        : r.kind,
  );

test("a plan as it usually is: its steps on the goal's line, two that run at once on a branch beside it", () => {
  const steps = [
    step(1, "Write the install matrix: every harness on every platform", [], "met"),
    step(2, "Install, update, uninstall and report versions on a runner", [1], "met"),
    step(3, "Manage harnesses per runner on the System page", [1], "met"),
    step(4, "Install from the New agents dialog and the model picker", [2, 3], "met"),
    step(5, "Verify on real runners and document", [4], "working"),
    step(6, "Release it", [5]),
  ];
  assert.deepEqual(said(goalPlanRows(steps)), ["1", "fork 2", "b2", "b3", "join", "4", "5", "6"]);
});

test("a crowded plan: its finished stretch folds, any number at once hang on one branch, and a step waiting further up says so", () => {
  const steps = [
    step(1, "Write the install matrix: every harness on every platform", [], "met"),
    step(2, "Detect each harness", [1], "met"),
    step(3, "Pick the install method", [2], "met"),
    step(4, "Write the scripts' common part", [3], "met"),
    step(5, "Script installs on Linux", [4], "met"),
    step(6, "Script installs on macOS", [4], "working"),
    step(7, "PowerShell installs on Windows", [4], "working"),
    step(8, "npm installs everywhere", [4], "needs_you"),
    step(9, "A Homebrew tap", [4], "skipped"),
    step(10, "The runner re-checks its harnesses after an install", [5, 6, 7, 8, 9]),
    step(11, "Live check on adacachy", [10], "unmet"),
    step(12, "Live check on a MacBook", [10]),
    step(13, "Live check on jupiter", [10]),
    step(14, "Document the matrix and the Windows ARM64 gap", [1]),
  ];
  assert.deepEqual(said(goalPlanRows(steps)), ["folded 4", "fork 5", "b5", "b6", "b7", "b8", "b9", "join", "10", "fork 3", "b11", "b12", "b13", "join", "14 after 1"]);
  // Shown, the stretch is there as it is.
  assert.deepEqual(said(goalPlanRows(steps, true)).slice(0, 5), ["1", "2", "3", "4", "fork 5"]);
  // Only a crowded plan folds, and only a stretch of more than one step.
  assert.equal(said(goalPlanRows(steps.slice(0, 8)))[0], "1");
});

test("a step that waits for nothing, further down, says it starts at once; one waiting for part of a branch says which", () => {
  const steps = [step(1, "One", []), step(2, "Two", [1]), step(3, "Three", [1]), step(4, "Four", [2]), step(5, "Five", [])];
  assert.deepEqual(said(goalPlanRows(steps)), ["1", "fork 2", "b2", "b3", "join", "4 after 2", "5 from the start"]);
});

test("another step's title, as a step speaks of it; a runner, as who's at a step says it", () => {
  assert.equal(shortTitle("Write the install matrix: every harness on every platform"), "Write the install matrix");
  assert.equal(shortTitle("Release it"), "Release it");
  assert.ok(shortTitle("A very long title that goes on and on past what anyone reads at a glance").endsWith("…"));
  assert.equal(machineWord("agent:fedora.attlocal.net"), "fedora");
  assert.equal(machineWord("adacachy"), "adacachy");
  assert.equal(machineWord("local"), "the server");
});
