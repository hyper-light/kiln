import assert from "node:assert/strict";
import { test } from "node:test";
import { cells, LIVELINESS, LOOK, toneAt } from "../src/views/harness/stands-light.ts";

// The light Mission control's rule, a compaction and the scales share (stands-light.ts): the rule's
// counts in their cells, and a grain's tone in its state's colour.

test("a count keeps its room: 9 to 10 moves nothing beside it, and only a changed place rolls, the way the count goes", () => {
  assert.deepEqual(cells(9, 10, "start"), [
    { was: "9", now: "1", dir: "up" },
    { was: "", now: "0", dir: "up" },
  ]);
  assert.deepEqual(cells(9, 10, "end"), [
    { was: "", now: "1", dir: "up" },
    { was: "9", now: "0", dir: "up" },
  ]);
  assert.deepEqual(cells(28, 27, "end"), [
    { was: "2", now: "2", dir: "down" },
    { was: "8", now: "7", dir: "down" },
  ]);
  assert.equal(cells(3, 3, "start").length, 2, "two places at least");
  assert.equal(cells(99, 100, "end").length, 3);
  assert.ok(cells(5, 5, "end").every((c) => c.was === c.now), "an unchanged count doesn't roll");
});

test("a grain wears its state's own colour, the ring only a tint in it, lighter or deeper by its tone", () => {
  const rgb = (c: number) => [(c >> 16) & 255, (c >> 8) & 255, c & 255] as const;
  const near = (a: readonly number[], b: readonly number[], most: number) => Math.max(...a.map((v, i) => Math.abs(v - (b[i] ?? 0)))) <= most;
  for (const stand of LIVELINESS) {
    const own = rgb(parseInt(LOOK[stand].colour.slice(1), 16));
    for (let s = 0; s < 840; s += 7)
      for (const t of [0, 3.3, 17.9]) {
        const c = rgb(toneAt(stand, s, t, 0, 0));
        assert.ok(near(c, own, 64), `${stand} at ${s} points, ${t}s: ${c} is near its own ${own}`);
      }
  }
  assert.ok(
    rgb(toneAt("needs", 50, 1, 0, 0)).every((v, i, [r, , b]) => i > 0 || r > (b ?? 0)),
    "needs you's gold",
  );
  const sum = (c: number) => rgb(c).reduce((a, b) => a + b, 0);
  assert.ok(
    sum(toneAt("working", 50, 1, 0, 1)) > sum(toneAt("working", 50, 1, 0, 0)) && sum(toneAt("working", 50, 1, 0, 0)) > sum(toneAt("working", 50, 1, 0, -1)),
    "lighter by its tone, deeper against it",
  );
  assert.ok(sum(toneAt("finished", 50, 1, 0, 0, 1)) > sum(toneAt("finished", 50, 1, 0, 0)), "lifted toward the glint as it's lit");
});
