import assert from "node:assert/strict";
import { test } from "node:test";
import { pageInGap, revealRow, rowWindow, survivingPosition, type RowPosition } from "../src/views/harness/row-window.ts";

const keys = Array.from({ length: 2000 }, (_, i) => `call-${i}`);
const shown = (all: string[], anchor: string | null, pins = new Set<string>()) => rowWindow(all, 60, anchor, pins).parts.flatMap((part) => (part.type === "rows" ? all.slice(part.from, part.to) : []));

test("W21-R02 a giant single grouped step mounts60 actual call rows and every earlier/later row remains reachable", () => {
  let anchor: string | null = null;
  const visited = new Set<string>();
  for (let i = 0; i < 40; i++) {
    const window = rowWindow(keys, 60, anchor);
    const mounted = shown(keys, anchor);
    assert.ok(mounted.length <= 60);
    mounted.forEach((key) => visited.add(key));
    const earlier = window.parts.find((part) => part.type === "gap" && part.to <= window.from);
    if (!earlier) break;
    anchor = pageInGap(keys, earlier, 60, window.from);
  }
  assert.equal(visited.size, keys.length);
  assert.equal(anchor, keys[0]);
  for (let i = 0; i < 40; i++) {
    const window = rowWindow(keys, 60, anchor);
    const later = window.parts.find((part) => part.type === "gap" && part.from >= window.to);
    if (!later) break;
    anchor = pageInGap(keys, later, 60, window.from);
  }
  assert.ok(shown(keys, anchor).includes(keys.at(-1)!));
});

test("W21-R02 finding an old call reveals its bounded page instead of mounting all intervening rows", () => {
  for (const target of [keys[0]!, keys[100]!, keys[999]!, keys.at(-1)!]) {
    const anchor = revealRow(keys, target);
    const mounted = shown(keys, anchor);
    assert.ok(mounted.includes(target));
    assert.ok(mounted.length <= 60);
  }
  assert.equal(revealRow(keys, "deleted"), null);
});

test("W21-R02 selection/focus/pending exceptions remain mounted and ordered as a live turn grows", () => {
  const pins = new Set([keys[4]!, keys[100]!, keys[1500]!]);
  const mounted = shown(keys, null, pins);
  assert.equal(mounted.length, 63);
  for (const id of pins) assert.ok(mounted.includes(id));
  const growing = [...keys, "new-streamed-call"];
  const next = shown(growing, null, pins);
  for (const id of pins) assert.ok(next.includes(id));
  assert.deepEqual(
    next,
    next.slice().sort((a, b) => growing.indexOf(a) - growing.indexOf(b)),
  );
  assert.equal(shown(growing, null).length, 60, "released interactions no longer hold off-page rows");
});

test("W21-R02 key anchors survive history prepend, live edits, deletion and reorder without copied event content", () => {
  const anchor = keys[1000]!;
  const before = shown(keys, anchor);
  assert.deepEqual(shown(["older-a", "older-b", ...keys], anchor), before);
  assert.deepEqual(shown(keys.slice(), anchor), before);
  assert.ok(shown([...keys.slice(0, 1000), ...keys.slice(1001)], anchor).includes(keys.at(-1)!), "deleted anchor safely falls back to the latest page");
  const reordered = [keys[1001]!, ...keys.filter((key) => key !== keys[1001])];
  assert.equal(shown(reordered, anchor)[0], anchor);
});

test("W21-R02 legacy outer-header positions and new actual-row positions restore independently", () => {
  const legacy: RowPosition = { id: "call-1", offset: -42 };
  const modern: RowPosition = {
    ...legacy,
    row: true,
    neighbors: [
      { id: "call-2", row: true },
      { id: "call-0", row: true },
    ],
  };
  const has = (id: string, row?: true) => (id === "call-1" && row !== true) || (id === "call-2" && row === true);
  assert.equal(survivingPosition(legacy, has), legacy);
  assert.deepEqual(survivingPosition(modern, has), { ...modern, id: "call-2" });
  assert.deepEqual(JSON.parse(JSON.stringify(legacy)), legacy, "legacy persisted JSON needs no migration");
  assert.deepEqual(survivingPosition(JSON.parse(JSON.stringify(modern)), has), { ...modern, id: "call-2" });
  assert.equal(
    survivingPosition(modern, () => false),
    null,
  );
  assert.equal(
    survivingPosition({ ...modern, neighbors: [...modern.neighbors!, { id: "not-a-bounded-neighbor" }] }, (id) => id === "not-a-bounded-neighbor"),
    null,
  );
});
