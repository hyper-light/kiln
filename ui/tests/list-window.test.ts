/** W21-R02: the arithmetic of a windowed list of rows of differing heights (src/lib/list-window.ts), and its window off the page. */
import assert from "node:assert/strict";
import { test } from "node:test";
import { ListWindow, mountedRuns, nearRows, rowAt, rowTops, shiftAbove, steadyBy, typicalHeight, windowParts, type Part } from "../src/lib/list-window.ts";

const keys = (n: number) => Array.from({ length: n }, (_, i) => `row:${i}`);
/** Rows of 84 px, every seventh 141 px, every fiftieth with 600 px open under it: a list like Mission control's. */
const tall = (i: number) => (i % 50 === 49 ? 684 : i % 7 === 6 ? 141 : 84);
const measured = (all: readonly string[], only: (i: number) => boolean = () => true) => new Map(all.flatMap((k, i) => (only(i) ? [[k, tall(i)] as const] : [])));
const rows = (parts: readonly Part[]) => parts.flatMap((p) => ("row" in p ? [p.row] : []));
/** The list as drawn for a viewport, as the window draws it. */
function drawn(all: readonly string[], heights: ReadonlyMap<string, number>, viewTop: number, viewHeight: number, keep: number[] = []) {
  const tops = rowTops(all, heights, typicalHeight(heights.values()) ?? 3 * viewHeight);
  return { tops, parts: windowParts(all, tops, mountedRuns(nearRows(tops, viewTop, viewHeight), keep)) };
}

test("W21-R02 a row never measured takes the middle of the heights measured, which one tall row among them doesn't move", () => {
  assert.equal(typicalHeight([]), undefined);
  assert.equal(typicalHeight([84]), 84);
  assert.equal(typicalHeight([84, 141]), 112.5);
  assert.equal(typicalHeight([141, 84, 84, 84, 9000]), 84);
  assert.equal(typicalHeight([84, 84, 31, 84, 84, 684, 84]), 84);
});

test("W21-R02 each row's top is the heights before it, the measured ones' own and the typical one for the rest", () => {
  const all = keys(5);
  const tops = rowTops(
    all,
    new Map([
      ["row:1", 100],
      ["row:3", 30],
    ]),
    84,
  );
  assert.deepEqual([...tops], [0, 84, 184, 268, 298, 382]);
  assert.equal(rowAt(tops, 0), 0);
  assert.equal(rowAt(tops, 83.9), 0);
  assert.equal(rowAt(tops, 84), 1, "a row ends where the next begins");
  assert.equal(rowAt(tops, 297), 3);
  assert.equal(rowAt(tops, 382), 5, "past them all");
  assert.equal(rowAt(tops, -50), 0);
});

test("W21-R02 the rows mounted are those within a viewport of the viewport, whatever their heights", () => {
  const all = keys(2000);
  const heights = measured(all);
  const tops = rowTops(all, heights, 84);
  for (const [viewTop, viewHeight] of [
    [0, 900],
    [-600, 900],
    [12_345, 839],
    [(tops[2000] ?? 0) - 400, 900],
  ] as const) {
    const [from, to] = nearRows(tops, viewTop, viewHeight);
    for (let i = 0; i < all.length; i++) {
      const inReach = (tops[i + 1] ?? 0) > viewTop - viewHeight && (tops[i] ?? 0) < viewTop + 2 * viewHeight;
      assert.equal(i >= from && i < to, inReach, `row ${i} with the viewport at ${viewTop}`);
    }
  }
  // A viewport nowhere near the list mounts none of it.
  assert.deepEqual(nearRows(tops, -5000, 900), [0, 0]);
  assert.deepEqual(nearRows(tops, (tops[2000] ?? 0) + 5000, 900), [2000, 2000]);
});

test("W21-R02 the gaps keep the page as tall and every mounted row where it would be with all of them mounted", () => {
  const all = keys(5000);
  const heights = measured(all);
  const whole = rowTops(all, heights, 84);
  for (const viewTop of [0, 700, 40_000, 199_000, (whole[5000] ?? 0) - 900]) {
    const { parts } = drawn(all, heights, viewTop, 900, [3, 2500, 4999]);
    let y = 0;
    for (const p of parts) {
      if ("row" in p) {
        assert.equal(y, whole[p.row], `row ${p.row}'s top`);
        y += tall(p.row);
      } else y += p.room;
    }
    assert.equal(y, whole[5000], "the list's height");
    // A fraction of the list is mounted: three viewports of rows, and the three kept.
    assert.ok(rows(parts).length <= Math.ceil((3 * 900) / 84) + 1 + 3, `${rows(parts).length} rows mounted`);
    for (const kept of [3, 2500, 4999]) assert.ok(rows(parts).includes(kept), `row ${kept} is kept`);
  }
});

test("W21-R02 rows kept wherever they are make runs of their own, joined to what they touch", () => {
  assert.deepEqual(mountedRuns([10, 20]), [[10, 20]]);
  assert.deepEqual(mountedRuns([10, 20], [0, 5, 9, 15, 20, 21, 40, 5]), [
    [0, 1],
    [5, 6],
    [9, 22],
    [40, 41],
  ]);
  assert.deepEqual(mountedRuns([7, 7], [2]), [[2, 3]], "nothing near: only what's kept");
  assert.deepEqual(mountedRuns([7, 7]), []);
  // A gap is named by the row after it ("" at the list's end), and none is drawn for rows that take no room.
  const all = keys(6);
  const tops = rowTops(all, new Map([["row:0", 0]]), 50);
  assert.deepEqual(
    windowParts(all, tops, [
      [1, 2],
      [4, 5],
    ]),
    [
      { key: "row:1", row: 1 },
      { gap: "row:4", room: 100 },
      { key: "row:4", row: 4 },
      { gap: "", room: 50 },
    ],
  );
});

test("W21-R02 drawing the window anew never moves a mounted row: what stays on the page, gaps and rows, keeps its order", () => {
  const all = keys(400);
  const heights = measured(all);
  /** Each part by the key React has it by. */
  const named = (parts: readonly Part[]) => parts.map((p) => ("row" in p ? p.key : `gap:${p.gap}`));
  // Down the list and back, a third of a viewport at a time, with rows kept wherever the page goes (the first,
  // one with the focus, the last); and once from the top straight to its foot.
  const tops = rowTops(all, heights, 84);
  const foot = (tops[400] ?? 0) - 900;
  const stops = [...Array.from({ length: 120 }, (_, i) => i * 300), ...Array.from({ length: 120 }, (_, i) => (119 - i) * 300), foot, 0];
  let last: string[] = [];
  for (const viewTop of stops) {
    const now = named(drawn(all, heights, viewTop, 900, [0, 120, 399]).parts);
    // What was on the page and still is, in the same order before and after: React moves none of it.
    const was = new Set(last);
    const is = new Set(now);
    assert.deepEqual(
      now.filter((k) => was.has(k)),
      last.filter((k) => is.has(k)),
      `at ${viewTop}`,
    );
    last = now;
  }
  // The rows near the top leaving the window: the gap that comes before what stays is new, the one after it is the one that was.
  const at = (viewTop: number) => named(drawn(all, heights, viewTop, 900, [0, 399]).parts);
  assert.deepEqual(
    at(0).filter((k) => k.startsWith("gap:")),
    ["gap:row:399"],
  );
  const lower = at(2000);
  assert.equal(lower[1], `gap:${lower[2]}`);
  assert.equal(lower.at(-2), "gap:row:399");
});

test("W21-R02 what's in view stays where it is: the page is scrolled by hand by whatever the browser didn't scroll by itself", () => {
  // A tree above what's in view grew by 7,165 px: the browser anchoring the scroll scrolled by as much (to the pixel), or anchors to the list
  // itself (after a jump past the rows on the page, until the next scroll) and didn't.
  assert.equal(steadyBy(7165, 7165, true, false), 0);
  assert.equal(steadyBy(7165, 7164.6, true, false), 0);
  assert.equal(steadyBy(7165, 0, true, false), 7165);
  // The gaps above it drawn 1,958 px shorter near the page's foot: the browser only pulled the page back 993 px to the foot that rose.
  assert.equal(steadyBy(-1958, -993, true, true), -965);
  // Without anchoring: all of it, from where the page was pulled back to if it was, on top of any scroll since otherwise.
  assert.equal(steadyBy(9734, 0, false, false), 9734);
  assert.equal(steadyBy(-2191, -2024, false, true), -167);
  assert.equal(steadyBy(500, 1200, false, false), 500);
  // Scrolled since by something else, where the browser anchors: left as it is (it may have anchored for it). Nothing due: nothing.
  assert.equal(steadyBy(324, 700, true, false), 0);
  assert.equal(steadyBy(0, -300, false, true), 0);
});

test("W21-R02 scrolling a long list top to bottom mounts every row in its turn, never more than a window's worth", () => {
  const all = keys(2000);
  // Rows are measured as they're first mounted, as on the page.
  const heights = new Map<string, number>();
  const seen = new Set<number>();
  let most = 0;
  let viewTop = 0;
  for (;;) {
    const { tops, parts } = drawn(all, heights, viewTop, 839);
    const mounted = rows(parts);
    most = Math.max(most, mounted.length);
    for (const i of mounted) {
      seen.add(i);
      heights.set(all[i] ?? "", tall(i));
    }
    if (viewTop >= (tops[2000] ?? 0) - 839) break;
    viewTop += 839;
  }
  assert.equal(seen.size, 2000);
  assert.ok(most <= Math.ceil((3 * 839) / 84) + 1, `${most} rows mounted at once`);
});

test("W21-R02 nothing measured yet, one row is taken to fill the window: the first is mounted, then those it shows room for", () => {
  const all = keys(500);
  const first = drawn(all, new Map(), -620, 900);
  assert.deepEqual(rows(first.parts), [0]);
  const second = drawn(all, new Map([["row:0", 84]]), -620, 900);
  assert.deepEqual(
    rows(second.parts),
    Array.from({ length: 15 }, (_, i) => i),
    "the rows down to a viewport under the viewport",
  );
});

test("W21-R02 what's in view moves by what the rows above it gained, and not at all for rows in it or under it", () => {
  const all = keys(100);
  const was = rowTops(all, new Map(), 84);
  // The viewport's top edge is in row 50: rows 10 and 20, measured taller, push it down; row 50 and row 70 don't.
  const y = 50 * 84 + 10;
  const now = rowTops(
    all,
    new Map([
      ["row:10", 141],
      ["row:20", 100],
      ["row:50", 300],
      ["row:70", 200],
    ]),
    84,
  );
  assert.equal(shiftAbove(was, now, y), 141 - 84 + (100 - 84));
  assert.equal(shiftAbove(was, rowTops(all, new Map([["row:50", 300]]), 84), y), 0);
  // A typical height that changes moves it by every unmeasured row above.
  assert.equal(shiftAbove(was, rowTops(all, new Map(), 85), y), 50);
  assert.equal(shiftAbove(was, now, -40), 0, "the list's top under the viewport's: nothing is above");
});

test("W21-R02 W26-R15 off the page a window mounts what's kept and the rows Tab starts and ends in, and the same rows give the same view", () => {
  const win = new ListWindow();
  const all = ["sep:pinned", ...keys(50), "sep:rest", ...keys(60).slice(50)];
  const opts = { attr: "data-entry", pinned: new Set(["row:30", "gone"]), tabStop: (key: string) => !key.startsWith("sep:") };
  win.sync(all, opts);
  const view = win.get();
  assert.deepEqual(
    view.parts.flatMap((p) => ("row" in p ? [p.key] : [])),
    ["row:0", "row:30", "row:59"],
  );
  // The same keys in a new array, as each render makes them: nothing to draw again.
  win.sync([...all], opts);
  assert.equal(win.get(), view);
  win.sync(all, { ...opts, pinned: new Set(["row:31"]) });
  assert.deepEqual(
    win.get().parts.flatMap((p) => ("row" in p ? [p.key] : [])),
    ["row:0", "row:31", "row:59"],
  );
  assert.equal(win.heightOf("row:0"), undefined, "nothing measured off the page");
  assert.equal(win.reveal("nowhere"), false);
});
