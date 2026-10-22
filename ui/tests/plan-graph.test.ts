import assert from "node:assert/strict";
import { test } from "node:test";
import { effectiveNeeds, rounds } from "../src/views/plans/graph-layout.ts";

const step = (n: number, needs: number[]) => ({ n, title: `s${n}`, needs });

test("steps fall in rounds after the latest step they wait for, side by side within one", () => {
  const graph = [step(1, []), step(2, [1]), step(3, [1]), step(4, []), step(5, [2, 3])];
  assert.deepEqual(
    rounds(graph).map((r) => r.map((s) => s.n)),
    [[1, 4], [2, 3], [5]],
  );
  // A chain, one a round; a step that waits for one not there waits for nothing.
  assert.deepEqual(
    rounds([step(1, []), step(2, [1]), step(3, [2])]).map((r) => r.map((s) => s.n)),
    [[1], [2], [3]],
  );
  assert.deepEqual(
    rounds([step(1, [9])]).map((r) => r.map((s) => s.n)),
    [[1]],
  );
});

test("a plan whose steps say nothing of it runs each after the one before", () => {
  assert.deepEqual(effectiveNeeds([undefined, undefined, []]), [[], [1], [2]]);
  assert.deepEqual(effectiveNeeds([[], [1], [1]]), [[], [1], [1]]);
  assert.deepEqual(effectiveNeeds([undefined, [1], undefined]), [[], [1], []], "once one says, each says for itself");
});
