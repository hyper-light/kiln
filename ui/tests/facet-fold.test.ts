import assert from "node:assert/strict";
import { test } from "node:test";
import { foldChips, MIN_CUT, MIN_ONE, sameFold, typingRoom, UNFOLDED } from "../src/components/ui/facet-fold.ts";

// A filter field's one line (facet-fold.ts): as many chips as fit, room kept to type, the rest in "+N".

const GAP = 6;
const MORE = 30; // "+3"
const COUNT = 70; // "4 filters"
// Mission control on a 430 pt phone: a 240 px field leaves its chips a 161 px line.
const PHONE = 161;
const DEVIN = 66;
const FOUR = [85, 72, 66, 92]; // Working, jupiter, Devin, Sonnet 4.5
const fold = (line: number, chips: number[], typed = 16) => foldChips({ line, chips, more: MORE, count: COUNT, typed, gap: GAP });

test("every chip shows while they all fit with room to type", () => {
  assert.deepEqual(fold(PHONE, [DEVIN]), { shown: 1 });
  assert.deepEqual(fold(600, FOUR), { shown: 4 });
  assert.deepEqual(fold(PHONE, []), { shown: 0 });
});

test("what doesn't fit folds into +N after the last chip that does", () => {
  assert.deepEqual(fold(PHONE, FOUR), { shown: 1 }, "Working, then +3");
  assert.deepEqual(fold(300, FOUR), { shown: 3 }, "three, then +1");
  // The +N chip and the room to type are both kept.
  const line = FOUR[0]! + GAP + FOUR[1]! + GAP + MORE + GAP + 16;
  assert.deepEqual(fold(line, FOUR), { shown: 2 });
  assert.deepEqual(fold(line - 1, FOUR), { shown: 1 });
});

test("a narrower line never shows more chips", () => {
  let last = Number.POSITIVE_INFINITY;
  for (let line = 600; line >= 40; line -= 7) {
    const f = fold(line, FOUR);
    assert.ok(f.shown <= last, `${line} px shows ${f.shown}, after ${last}`);
    last = f.shown;
  }
});

test("the only chip is cut to leave room to type, never below a readable width", () => {
  assert.deepEqual(fold(120, [180], 40), { shown: 1, cut: 120 - 40 - GAP });
  assert.deepEqual(fold(60, [180], 40), { shown: 1, cut: MIN_ONE });
});

test("when no chip fits beside +N, the first is cut while it still reads, else one chip counts them all", () => {
  const cut = fold(MORE + GAP + 16 + GAP + MIN_CUT, [200, 200]);
  assert.deepEqual(cut, { shown: 1, cut: MIN_CUT });
  assert.deepEqual(fold(MORE + GAP + 16 + GAP + MIN_CUT - 1, [200, 200]), { shown: 0, count: "words" });
  // Too narrow even for "2 filters" and the words typed: the count alone.
  assert.deepEqual(fold(80, [200, 200], 72), { shown: 0, count: "plus" });
});

test("the room kept to type is a caret's worth, or the words typed within bounds", () => {
  assert.equal(typingRoom(161, 0, false), 16);
  assert.equal(typingRoom(161, 20, true), 72, "at least 72 px once typing");
  assert.equal(typingRoom(161, 80, true), 94);
  assert.equal(typingRoom(161, 400, true), 161 * 0.6, "at most 60% of the line");
});

test("folds compare by what they show", () => {
  assert.ok(sameFold({ shown: 2 }, { shown: 2 }));
  assert.ok(!sameFold({ shown: 1, cut: 90 }, { shown: 1 }));
  assert.ok(!sameFold(UNFOLDED, { shown: 4 }));
});
