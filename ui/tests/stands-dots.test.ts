import assert from "node:assert/strict";
import { test } from "node:test";
import { between, dotsFrame, layout, PITCH, type Slot } from "../src/views/harness/stands-dots.ts";
import type { Shown } from "../src/views/harness/stands-light.ts";

// Mission control's rule as its agents (stands-dots.ts): a dot for each, banded by state, each band's
// arrowhead fitting the notch of the one before, each band playing a game of life of its own.

const ORDER: Shown[] = ["needs", "crashed", "paused", "working", "finished", "ready", "offline"];
const bands = (c: Partial<Record<Shown, number>>) => ORDER.map((s) => [s, c[s] ?? 0] as const);
const SCREEN = bands({ needs: 7, working: 11, finished: 50, offline: 24 });
const EVERY = bands({ needs: 2, crashed: 1, paused: 1, working: 6, finished: 14, ready: 2, offline: 1 });
const PHONE = 382;
const DESKTOP = 920;
const of = (slots: readonly Slot[], s: Shown) => slots.filter((d) => d.stand === s);

test("a dot for each agent, banded in the order the ways read, every dot inside the rule", () => {
  for (const w of [PHONE, DESKTOP])
    for (const b of [SCREEN, EVERY]) {
      const l = layout(b, w);
      assert.equal(
        l.slots.length,
        b.reduce((s, [, n]) => s + n, 0),
        "one dot an agent",
      );
      for (const [stand, n] of b)
        assert.deepEqual(
          of(l.slots, stand)
            .map((d) => d.i)
            .sort((x, y) => x - y),
          [...Array(n).keys()],
          `${stand}: each of its ${n} once`,
        );
      for (const d of l.slots) assert.ok(d.x >= 4 && d.x <= w - 4 && d.y >= 4 && d.y <= l.height - 4, `${d.stand} ${d.i} inside (${d.x.toFixed(1)}, ${d.y.toFixed(1)})`);
      const shown = b.filter(([, n]) => n > 0).map(([s]) => s);
      shown.slice(1).forEach((s, k) => {
        const a = of(l.slots, shown[k]!);
        assert.ok(Math.min(...of(l.slots, s).map((d) => d.x)) > Math.min(...a.map((d) => d.x)), `${s} after ${shown[k]}`);
      });
    }
});

test("each band's left side is an arrowhead and its right side the notch the next one's fits, a column clear between them on every row", () => {
  const l = layout(SCREEN, PHONE);
  const mid = (l.rows - 1) / 2;
  const shown = SCREEN.filter(([, n]) => n > 0).map(([s]) => s);
  for (const [s, n] of SCREEN.filter(([, n]) => n > 0)) {
    const dots = of(l.slots, s);
    const first = Math.min(...dots.map((d) => d.x));
    for (const d of dots.filter((d) => d.col === 0)) assert.ok(Math.abs(d.x - first - Math.abs(d.row - mid) * l.pitch) < 1e-9, `${s}: its arrowhead, row ${d.row}`);
    // Its last whole column is the same shape: the notch.
    const whole = Math.floor(n / l.rows) - 1;
    const notch = dots.filter((d) => d.col === whole);
    const back = Math.min(...notch.map((d) => d.x));
    for (const d of notch) assert.ok(Math.abs(d.x - back - Math.abs(d.row - mid) * l.pitch) < 1e-9, `${s}: its notch, row ${d.row}`);
  }
  shown.slice(1).forEach((s, k) => {
    const a = of(l.slots, shown[k]!);
    const b = of(l.slots, s);
    for (let r = 0; r < l.rows; r++) {
      const end = Math.max(...a.filter((d) => d.row === r).map((d) => d.x));
      const start = Math.min(...b.filter((d) => d.row === r).map((d) => d.x));
      assert.ok(start - end >= 2 * l.pitch - 1e-9, `row ${r}: ${s} starts a clear column after ${shown[k]}`);
    }
  });
});

test("more agents than fit across: every band grows two rows, so the arrowheads keep their point", () => {
  assert.equal(layout(SCREEN, PHONE).rows, 3);
  const many = bands({ needs: 20, working: 80, finished: 200 });
  const l = layout(many, PHONE);
  assert.ok(l.rows > 3 && l.rows % 2 === 1, `${l.rows} rows, odd`);
  assert.ok(Math.max(...l.slots.map((d) => d.x)) <= PHONE - 4, "still inside");
  assert.ok(l.height > layout(SCREEN, PHONE).height, "and taller");
  assert.ok(l.pitch >= PITCH, "never closer than a pitch");
});

test("a change of counts moves each dot to its new place, the new ones coming in and the gone ones going", () => {
  const a = layout(SCREEN, PHONE);
  const b = layout(bands({ needs: 6, working: 12, finished: 50, offline: 24 }), PHONE);
  const at = (k: number) => between(a, b, k).dots;
  const key = (d: Slot) => `${d.stand}:${d.i}`;
  assert.deepEqual(
    at(0)
      .filter((d) => d.on > 0)
      .map((d) => [key(d), d.x, d.y])
      .sort(),
    a.slots.map((d) => [key(d), d.x, d.y]).sort(),
    "at the start, where they were",
  );
  assert.deepEqual(
    at(1)
      .filter((d) => d.on > 0)
      .map((d) => [key(d), d.x, d.y])
      .sort(),
    b.slots.map((d) => [key(d), d.x, d.y]).sort(),
    "at the end, where they are",
  );
  const half = at(0.5);
  assert.ok(
    half.some((d) => d.stand === "needs" && d.i === 6 && d.on > 0 && d.on < 1),
    "needs you's seventh going",
  );
  assert.ok(
    half.some((d) => d.stand === "working" && d.i === 11 && d.on > 0 && d.on < 1),
    "working's twelfth coming",
  );
});

/** Over a few seconds, how lit each of a band's dots is (its middle grain's opacity), a frame every `dt`. */
function litOver(b: ReturnType<typeof bands>, stand: Shown, t0: number, seconds: number, dt: number) {
  const l = layout(b, DESKTOP);
  const dots = between(l, l, 1).dots;
  const out: number[][] = [];
  for (let t = t0; t < t0 + seconds; t += dt) {
    const f = dotsFrame(dots, l.rows, t);
    const by = new Map<string, number>();
    // A dot's middle grain is its first; another may pass over the same place later.
    for (let k = 0; k < f.dots.length; k += 5) if (!by.has(`${f.dots[k]!.toFixed(3)},${f.dots[k + 1]!.toFixed(3)}`)) by.set(`${f.dots[k]!.toFixed(3)},${f.dots[k + 1]!.toFixed(3)}`, f.dots[k + 3]!);
    out.push(of(l.slots, stand).map((d) => by.get(`${d.x.toFixed(3)},${d.y.toFixed(3)}`) ?? 0));
  }
  return out;
}

test("each band plays a game of life: some dots lit and some dim, the pattern changing as it goes, working's quicker than finished's", () => {
  const changes = (frames: number[][]) => frames.slice(1).filter((f, k) => f.some((a, i) => Math.abs(a - (frames[k]![i] ?? 0)) > 0.2)).length;
  const working = litOver(SCREEN, "working", 100, 6, 0.1);
  const finished = litOver(SCREEN, "finished", 100, 6, 0.1);
  for (const [name, frames] of [
    ["working", working],
    ["finished", finished],
  ] as const) {
    const all = frames.flat();
    assert.ok(Math.max(...all) - Math.min(...all) > 0.3, `${name}: lit and dim dots`);
  }
  assert.ok(changes(working) > 2 * changes(finished), `working's pattern changes more often (${changes(working)} frames to ${changes(finished)})`);
  // Over a minute and more its patterns never settle: the lit set keeps changing.
  const late = litOver(SCREEN, "working", 4000, 6, 0.32);
  assert.ok(changes(late) > 6, `still changing an hour on (${changes(late)})`);
});

test("paused holds its pattern, and stopped and offline play none", () => {
  const b = bands({ paused: 9, crashed: 4, offline: 6 });
  for (const s of ["paused", "crashed", "offline"] as const) {
    const frames = litOver(b, s, 50, 4, 0.25);
    for (const f of frames.slice(1)) f.forEach((a, i) => assert.ok(Math.abs(a - (frames[0]![i] ?? 0)) < 0.12, `${s}'s dot ${i} holds`));
  }
});

test("every grain stays in the rule's box, the same moment drawn the same, and a frame is quick to make", () => {
  for (const [b, w] of [
    [SCREEN, PHONE],
    [EVERY, DESKTOP],
    [bands({ working: 120, finished: 180 }), PHONE],
  ] as const) {
    const l = layout(b, w);
    const dots = between(l, l, 1).dots;
    for (const t of [0, 1.3, 77.7]) {
      const f = dotsFrame(dots, l.rows, t);
      for (let k = 0; k < f.dots.length; k += 5) {
        const [x, y, r, a] = [f.dots[k]!, f.dots[k + 1]!, f.dots[k + 2]!, f.dots[k + 3]!];
        assert.ok(x - r >= 0 && x + r <= w && y - r >= 0 && y + r <= l.height, `(${x.toFixed(1)}, ${y.toFixed(1)}) r ${r.toFixed(2)} in the box`);
        assert.ok(a > 0 && a <= 1 && r > 0 && r < 1.5, "seen, and fine");
      }
      assert.deepEqual(dotsFrame(dots, l.rows, t).dots, f.dots, "drawn the same");
    }
  }
  const l = layout(bands({ needs: 20, working: 80, finished: 200 }), DESKTOP);
  const dots = between(l, l, 1).dots;
  const begun = performance.now();
  for (let t = 0; t < 2; t += 0.1) dotsFrame(dots, l.rows, t);
  const each = (performance.now() - begun) / 20;
  assert.ok(each < 10, `${each.toFixed(2)} ms a frame for 300 agents`);
});
