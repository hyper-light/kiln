import assert from "node:assert/strict";
import { test } from "node:test";
import { blocked, curve, effectiveNeeds, rounds } from "../src/views/plans/graph-layout.ts";

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

test("a line that would run behind another step isn't drawn: that step says what it waits for", () => {
  // A phone's rows of two steps: 1 and 2, then 3 and 4, then 5. A line from 1 down to 5 runs behind 3.
  const at = (left: number, top: number) => ({ left, top, right: left + 150, bottom: top + 50 });
  const [one, three, five] = [at(0, 0), at(0, 70), at(0, 140)];
  const mid = (b: { left: number; right: number }) => (b.left + b.right) / 2;
  assert.equal(blocked(curve(mid(one), one.bottom, mid(five), five.top, false), [three]), true);
  // Down to the step right under it: nothing in between.
  assert.equal(blocked(curve(mid(one), one.bottom, mid(three), three.top, false), [five]), false);
  // Across to the next round, past a step above the line: drawn.
  const [a, b, above] = [at(0, 100), at(200, 100), at(100, 0)];
  assert.equal(blocked(curve(a.right, 125, b.left, 125, true), [above]), false);
  // The ends of a line touch its own steps' edges, never another's inside.
  assert.deepEqual(curve(0, 0, 10, 20, false, 2)[0], [0, 0]);
  assert.deepEqual(curve(0, 0, 10, 20, false, 2)[2], [10, 20]);
});
