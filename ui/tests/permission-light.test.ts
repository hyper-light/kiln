import assert from "node:assert/strict";
import { test } from "node:test";
import { Y } from "../src/views/harness/effort-light.ts";
import { growFronts, LIMIT, permissionFrame, restFronts } from "../src/views/harness/permission-light.ts";
import type { Frame } from "../src/views/harness/stands-light.ts";

// The permissions scale's lit track (permission-light.ts): a bramble from the scale's start to its
// knob, the more grown and snarled the further along (the more the agent may do on its own), its
// runners creeping past the knob, as run-settings.tsx draws it.

const TRACK = 300;
/** Six stops, as Claude Code's modes are, the last turning checks off. */
const STOPS = [0, 1, 2, 3, 4, 5].map((i) => (i / 5) * TRACK);
const WARN = [false, false, false, false, false, true];
const TIMES = [0.4, 1.7, 3.1, 5.9, 8.3, 12.6];
const stop = (i: number) => STOPS[i] ?? 0;

type Dot = { x: number; y: number; r: number; a: number; c: number };
function dotsOf(f: Frame) {
  const out: Dot[] = [];
  for (let k = 0; k + 4 < f.dots.length; k += 5) out.push({ x: f.dots[k] ?? 0, y: f.dots[k + 1] ?? 0, r: f.dots[k + 2] ?? 0, a: f.dots[k + 3] ?? 0, c: f.dots[k + 4] ?? 0 });
  return out;
}
const frameAt = (fill: number, t: number, fronts?: number[]) => permissionFrame(fill, t, { stops: STOPS, warn: WARN, fronts });
/** The bramble's own particles (the stops' dots are larger). */
const bramble = (fill: number, t: number, fronts?: number[]) => dotsOf(frameAt(fill, t, fronts)).filter((d) => d.r < 1.2);
const rgb = (c: number) => [(c >> 16) & 255, (c >> 8) & 255, c & 255] as const;
function quantile(xs: number[], p: number) {
  const s = [...xs].sort((a, b) => a - b);
  return s[Math.min(s.length - 1, Math.floor(s.length * p))] ?? 0;
}

/** Over several moments, the bramble's particles from `x0` to `x1` with the knob at `fill`: how many a frame, and how far from the rail nine in ten of them are. */
function measure(fill: number, x0: number, x1: number, fronts?: number[]) {
  let n = 0;
  const off: number[] = [];
  for (const t of TIMES) {
    for (const d of bramble(fill, t, fronts)) {
      if (d.x < x0 || d.x >= x1) continue;
      n++;
      off.push(Math.abs(d.y - Y));
    }
  }
  return { count: n / TIMES.length, spread: quantile(off, 0.9) };
}

test("it tapers out: from two fine canes at the start, each stretch further along has more of it and a wider tangle", () => {
  const fifths = [0, 1, 2, 3, 4].map((i) => measure(TRACK, i * 60, (i + 1) * 60));
  for (let i = 1; i < fifths.length; i++) {
    const [was, now] = [fifths[i - 1], fifths[i]];
    assert.ok(was && now);
    assert.ok(now.count > 1.4 * was.count, `fifth ${i}: ${now.count} particles a frame, after ${was.count}`);
    assert.ok(now.spread > was.spread + 0.5, `fifth ${i}: nine in ten within ${now.spread} of the rail, after ${was.spread}`);
  }
  assert.ok((fifths[0]?.spread ?? Infinity) < 1, "at the start, fine canes lying taut along the rail");
});

test("each stop up grows it more and more snarled at the knob", () => {
  const atKnob = [1, 2, 3, 4, 5].map((i) => measure(stop(i), stop(i) - 40, stop(i) + 0.5));
  for (let i = 1; i < atKnob.length; i++) {
    const [was, now] = [atKnob[i - 1], atKnob[i]];
    assert.ok(was && now);
    assert.ok(now.count > was.count, `stop ${i + 1}: ${now.count} particles a frame by the knob, after ${was.count}`);
    assert.ok(now.spread > was.spread, `stop ${i + 1}: nine in ten within ${now.spread} of the rail, after ${was.spread}`);
  }
});

test("it keeps clear of the stops' names under the track: nothing further than LIMIT from the rail, and most of it in a close core", () => {
  for (const [i, fill] of STOPS.entries()) {
    for (const t of TIMES) {
      const f = frameAt(fill, t);
      for (const d of dotsOf(f)) assert.ok(Math.abs(d.y - Y) <= LIMIT + 1e-9, `stop ${i} at ${t}s: a particle ${Math.abs(d.y - Y)} from the rail`);
      for (let k = 0; k + 5 < f.soft.length; k += 6)
        assert.ok(Math.abs((f.soft[k + 1] ?? Y) - Y) <= LIMIT + 1e-9, `stop ${i} at ${t}s: a soft light ${Math.abs((f.soft[k + 1] ?? Y) - Y)} from the rail`);
    }
  }
  assert.ok(LIMIT < 14, "the names' letters start about 16 under the rail");
  const core = measure(TRACK, TRACK - 60, TRACK + 0.5);
  assert.ok(core.spread < 8, `at the last stop, nine in ten by the knob within ${core.spread} of the rail`);
});

test("at the first stop there's no bramble yet", () => {
  for (const t of TIMES) {
    const canes = bramble(0, t);
    assert.ok(canes.length < 20, `${canes.length} particles at the first stop`);
    for (const d of canes) assert.ok(d.x < 15, `at the first stop, a particle ${d.x} along`);
  }
});

test("a step up grows its canes out to the knob: they reach only as far as they've grown, never faster than they grow", () => {
  const fronts = restFronts(stop(2), TRACK);
  for (const t of TIMES) {
    const furthest = Math.max(...bramble(stop(5), t, fronts).map((d) => d.x));
    assert.ok(furthest <= Math.max(...fronts) + 1, `just after the step up, it reaches ${furthest}, its canes ${Math.max(...fronts)}`);
  }
  let seconds = 0;
  while (fronts.some((x) => x < stop(5)) && seconds < 5) {
    const was = [...fronts];
    growFronts(fronts, stop(5), TRACK, 1 / 30);
    seconds += 1 / 30;
    for (const [v, x] of fronts.entries()) {
      const by = (x - (was[v] ?? 0)) * 30;
      assert.ok(by >= 0 && by <= 240 * 1.2 + 1e-9, `cane ${v} grew ${by} points a second`);
    }
  }
  assert.ok(seconds < 3, `grown out to the knob in ${seconds}s`);
  assert.deepEqual(fronts, restFronts(stop(5), TRACK), "grown, each cane's front at the knob (or where it sprouts)");
});

test("a step down shrivels it back: past the knob its canes wither, and shrink back to it", () => {
  const fronts = restFronts(stop(5), TRACK);
  const beyond = (f?: number[]) => {
    let light = 0;
    for (const t of TIMES) for (const d of bramble(stop(3), t, f)) if (d.x > stop(3) + 30) light += d.a;
    return light;
  };
  const grown = (() => {
    let light = 0;
    for (const t of TIMES) for (const d of bramble(stop(5), t)) if (d.x > stop(3) + 30) light += d.a;
    return light;
  })();
  const withering = beyond(fronts);
  assert.ok(withering < 0.5 * grown, `just after the step down, past the knob ${withering} light, grown there ${grown}`);
  let seconds = 0;
  while (fronts.some((x, v) => x !== restFronts(stop(3), TRACK)[v]) && seconds < 5) {
    const was = [...fronts];
    growFronts(fronts, stop(3), TRACK, 1 / 30);
    seconds += 1 / 30;
    for (const [v, x] of fronts.entries()) {
      const by = ((was[v] ?? 0) - x) * 30;
      assert.ok(by >= 0 && by <= 320 * 1.2 + 1e-9, `cane ${v} shrank ${by} points a second`);
    }
  }
  assert.ok(seconds < 3, `shrivelled back to the knob in ${seconds}s`);
  assert.ok(beyond(fronts) < 0.05 * grown, "shrivelled back, nothing of it past the knob but its runners");
});

test("past the knob its runners creep onto the track: the more and the further the higher the stop, never past its end, and only once its canes reach the knob", () => {
  const past = [0, 1, 2, 3, 4].map((i) => {
    let n = 0;
    let far = 0;
    for (const t of TIMES) {
      for (const d of bramble(stop(i), t)) {
        if (d.x <= stop(i) + 3) continue;
        n++;
        far = Math.max(far, d.x - stop(i));
        assert.ok(d.x <= TRACK, `a runner ${d.x} along, past the track's end`);
      }
    }
    return { n, far };
  });
  for (let i = 1; i < past.length; i++) {
    const [was, now] = [past[i - 1], past[i]];
    assert.ok(was && now);
    assert.ok(now.n > 1.3 * was.n, `stop ${i}: ${now.n} runners' particles, after ${was.n}`);
    assert.ok(now.far > was.far, `stop ${i}: runners reach ${now.far}, after ${was.far}`);
  }
  const growing = restFronts(stop(1), TRACK);
  for (const t of TIMES) assert.equal(bramble(stop(4), t, growing).filter((d) => d.x > stop(4) + 3).length, 0, "no runners while its canes are still growing out to the knob");
});

test("the stops: each it has reached a bead of its light; past the knob the scale's grey dots, warm where a stop turns checks off", () => {
  for (const t of TIMES) {
    const dots = dotsOf(frameAt(stop(2), t));
    const at = (x: number) => dots.filter((d) => Math.abs(d.x - x) < 1e-9 && d.r >= 1.3);
    for (const i of [0, 1]) {
      const [bead] = at(stop(i));
      assert.ok(bead, `stop ${i} reached: a bead`);
      const [r, g, b] = rgb(bead.c);
      assert.ok(bead.r >= 1.4 && bead.a > 0.8 && Math.max(r, g, b) > 180, `stop ${i}: a bead of its light (radius ${bead.r}, ${bead.a}, ${r},${g},${b})`);
    }
    const warmth: number[] = [];
    for (const i of [3, 4, 5]) {
      const [dot] = at(stop(i));
      assert.ok(dot, `stop ${i} past the knob: a dot`);
      const [r, g, b] = rgb(dot.c);
      assert.ok(dot.r >= 2.5 && dot.r < 3.2 && Math.max(r, g, b) < 140, `stop ${i}: the scale's grey dot (radius ${dot.r}, ${r},${g},${b})`);
      warmth.push(r - b);
    }
    // Each takes on the line's colours a little as it stirs (the nearest the most), the warm one still the warmest.
    const [, plain = 0, warm = 0] = warmth;
    assert.ok(warm > 0 && warm > plain + 10, `the stop that turns checks off is warm: red over blue ${warm}, the stop before it ${plain}`);
  }
});

test("past the knob the stops stir: the one nearest the knob the most, each further one less", () => {
  const moves = [3, 4, 5].map((i) => {
    const ys: number[] = [];
    for (let t = 0; t < 6; t += 0.05) {
      const dot = dotsOf(frameAt(stop(2), t)).find((d) => Math.abs(d.x - stop(i)) < 1e-9 && d.r >= 2.4);
      if (dot) ys.push(dot.y);
    }
    return Math.max(...ys) - Math.min(...ys);
  });
  const [nearest, next, last] = moves;
  assert.ok(nearest !== undefined && next !== undefined && last !== undefined);
  assert.ok(nearest > 0.5, `the nearest stirs ${nearest}`);
  assert.ok(nearest > 2 * next && next >= last * 0.9 && last > 0.1, `stirring ${moves.join(", ")}`);
});

test("it's always moving, and a moment is drawn the same each time", () => {
  const a = frameAt(TRACK, 3.3);
  const b = frameAt(TRACK, 3.3);
  assert.deepEqual(a.dots, b.dots);
  assert.deepEqual(a.soft, b.soft);
  const later = bramble(TRACK, 3.3 + 1 / 30);
  const now = bramble(TRACK, 3.3);
  assert.notDeepEqual(
    later.map((d) => d.y),
    now.map((d) => d.y),
  );
});

test("it costs about as much as the effort line: at most 16 particles a point of track at the last stop", () => {
  for (const track of [300, 600]) {
    const stops = STOPS.map((x) => (x / TRACK) * track);
    for (const t of TIMES) {
      const n = permissionFrame(track, t, { stops, warn: WARN }).dots.length / 5;
      assert.ok(n < 16 * track, `${n} particles on a track ${track} long`);
    }
  }
});
