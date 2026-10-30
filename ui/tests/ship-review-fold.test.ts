import assert from "node:assert/strict";
import { test } from "node:test";
import { fold, rowsOf, FIRST_LINES } from "../src/views/ship/review/fold.ts";

const hunk = (old_start: number, new_start: number, lines: string[]) => ({ old_start, old_lines: 0, new_start, new_lines: 0, lines });
const none = () => false;

test("ship plan step 12: each hunk with its context, a gap between hunks", () => {
  const rows = rowsOf({ hunks: [hunk(8, 8, [" a", "-b", "+B", " c"]), hunk(40, 40, [" d", "+e", " f"])] } as never);
  assert.deepEqual(
    rows.map((r) => [r.o, r.n, r.kind, r.h]),
    [
      [8, 8, "ctx", 0],
      [9, null, "del", 0],
      [null, 9, "add", 0],
      [10, 10, "ctx", 0],
      [40, 40, "ctx", 1],
      [null, 41, "add", 1],
      [41, 42, "ctx", 1],
    ],
  );
  assert.deepEqual(fold(rows, none), [0, 1, 2, 3, -1, 4, 5, 6]);
  // One hunk: every row, no gap.
  assert.deepEqual(fold(rowsOf({ hunks: [hunk(1, 1, ["+x", "+y"])] } as never), none), [0, 1]);
});

test("ship plan step 12: a long diff keeps its first lines and what carries something, one gap where both fold and hunks part", () => {
  const first = Array.from({ length: 60 }, (_, i) => `+a${i}`);
  const second = Array.from({ length: 60 }, (_, i) => `+b${i}`);
  const rows = rowsOf({ hunks: [hunk(1, 1, first), hunk(200, 200, second)] } as never);
  const shown = fold(rows, (_, i) => i === 70);
  const head = Array.from({ length: FIRST_LINES }, (_, i) => i);
  // Rows 12..69 fold away (the hunks part at 60, inside the fold: still one gap), then 70, then the rest.
  assert.deepEqual(shown, [...head, -(70 - FIRST_LINES), 70, -(rows.length - 71)]);
});
