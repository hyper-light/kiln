import assert from "node:assert/strict";
import { test } from "node:test";
import { cells, frame, HEIGHT, LIVE, LIVELINESS, reachOf, SHORT_OF_MIDDLE, stretches, stringOf, type Shown } from "../src/views/harness/stands-light.ts";

// Mission Control's status rule (stands-light.ts): the string, its particles and its stretches,
// as stands.tsx draws them each frame.

const ALL: Shown[] = ["needs", "crashed", "paused", "working", "finished", "ready", "offline"];
const COUNTS = [2, 1, 1, 6, 14, 2, 1];
const W = 390;
const near = (a: number, b: number, eps = 1e-9) => Math.abs(a - b) <= eps;
const TIMES = [0, 0.7, 3.3, 7.9];

test("the stretches fill the rule in order, each its share of the agents, a gap between each two", () => {
  const parts = stretches(ALL, COUNTS, W);
  assert.deepEqual(parts.map((p) => p.stand), ALL);
  assert.ok(near(parts[0]!.x0, 0) && near(parts[parts.length - 1]!.x1, W, 1e-6), "from end to end");
  parts.slice(1).forEach((p, i) => assert.ok(near(p.x0 - parts[i]!.x1, 5), `a gap of 5 after ${parts[i]!.stand}`));
  for (const p of parts) assert.ok(p.x1 - p.x0 >= 12 - 1e-9, `${p.stand} at least 12 long`);
  const len = (s: Shown) => parts.find((p) => p.stand === s)!.x1 - parts.find((p) => p.stand === s)!.x0;
  assert.ok(len("finished") > len("working") && len("working") > len("needs") && near(len("crashed"), len("paused")), "longer for more agents");
});

test("one string runs the whole rule unbroken, held at its two ends, and goes round every 8 s", () => {
  const parts = stretches(ALL, COUNTS, W);
  for (const t of TIMES) {
    const s = stringOf(parts, W, t);
    assert.ok(near(s.at(0), 0) && near(s.at(W), 0, 1e-6), `held at its ends at ${t}s`);
    let worst = 0;
    for (let x = 0; x < W; x += 0.25) worst = Math.max(worst, Math.abs(s.at(x + 0.25) - s.at(x)));
    // Its steepest wave (working's finest undamped: 0.2 high, 19.7 long) and the rest together
    // stay well under half a point of rise in a quarter point.
    assert.ok(worst < 0.5, `no break at ${t}s: its steepest quarter-point step is ${worst.toFixed(3)}`);
    // The same height either side of each gap, give or take its slope across five points, as one string would be.
    parts.slice(1).forEach((p, j) => {
      const a = parts[j]!;
      assert.ok(Math.abs(s.at(a.x1) - s.at(p.x0)) < 1.6, `${a.stand} meets ${p.stand} across its gap at ${t}s`);
    });
    const later = stringOf(parts, W, t + 8);
    for (let x = 0; x <= W; x += 13) assert.ok(near(s.at(x), later.at(x), 1e-6), `the same 8 s on, at ${x}`);
  }
});

test("each state damps the string as much as it's still: working the least, then ready, needs you, paused, finished, stopped; offline holds it still", () => {
  assert.deepEqual([...LIVELINESS].sort((a, b) => LIVE[b] - LIVE[a]), [...LIVELINESS]);
  // Alone across the rule: how far its middle swings, and how fast, over its 8 s.
  const swing = (s: Shown) => {
    const parts = stretches([s], [1], 300);
    let most = 0;
    let fastest = 0;
    for (let t = 0; t < 8; t += 0.05) {
      const now = stringOf(parts, 300, t);
      const next = stringOf(parts, 300, t + 0.05);
      for (let x = 100; x <= 200; x += 5) {
        most = Math.max(most, Math.abs(now.at(x)));
        fastest = Math.max(fastest, Math.abs(next.at(x) - now.at(x)) / 0.05);
      }
    }
    return { most, fastest };
  };
  const by = LIVELINESS.map((s) => ({ s, ...swing(s) }));
  by.slice(1).forEach((b, i) => {
    const a = by[i]!;
    assert.ok(b.most < a.most || (a.most === 0 && b.most === 0), `${b.s} swings less than ${a.s} (${b.most.toFixed(2)} < ${a.most.toFixed(2)})`);
    assert.ok(b.fastest < a.fastest || (a.fastest === 0 && b.fastest === 0), `${b.s} moves slower than ${a.s}`);
  });
  assert.equal(swing("offline").most, 0);
});

test("every particle stays in the rule's box, a fine point, seen", () => {
  const parts = stretches(ALL, COUNTS, W);
  for (const t of TIMES) {
    const f = frame(parts, W, t);
    assert.ok(f.dots.length > 5 * 500, "a fine, close line");
    for (let k = 0; k < f.dots.length; k += 5) {
      const [x, y, r, a] = [f.dots[k]!, f.dots[k + 1]!, f.dots[k + 2]!, f.dots[k + 3]!];
      assert.ok(x >= -0.5 && x <= W + 0.5, `x ${x} on the rule`);
      assert.ok(y - r >= 0 && y + r <= HEIGHT, `y ${y.toFixed(2)} in the box`);
      assert.ok(r > 0 && r < 1.5 && a > 0 && a <= 1, "a fine point, seen");
    }
    for (let k = 0; k < f.soft.length; k += 6) assert.ok(f.soft[k + 1]! >= 0 && f.soft[k + 1]! <= HEIGHT, "soft light about the string");
  }
});

test("working's particles float widest about the string, finished's close to it, offline's on it", () => {
  const spread = (s: Shown) => {
    let lo = Infinity;
    let hi = -Infinity;
    for (const t of TIMES) {
      const f = frame(stretches([s], [1], 300), 300, t);
      for (let k = 0; k < f.dots.length; k += 5) {
        lo = Math.min(lo, f.dots[k + 1]!);
        hi = Math.max(hi, f.dots[k + 1]!);
      }
    }
    return hi - lo;
  };
  assert.ok(spread("working") > spread("ready") && spread("ready") > spread("finished"), "working widest, finished closest");
  assert.ok(spread("offline") < 0.01, "offline's points lie on the still string");
});

test("at each gap both sides reach for each other, never touching", () => {
  const parts = stretches(ALL, COUNTS, W);
  for (const t of [0, 0.6, 1.2, 1.8, 2.4]) {
    assert.ok(reachOf(0, t) >= 0.35 - 1e-9 && reachOf(0, t) <= 1.6 + 1e-9);
    const f = frame(parts, W, t);
    parts.slice(1).forEach((p, j) => {
      const mid = (parts[j]!.x1 + p.x0) / 2;
      for (let k = 0; k < f.dots.length; k += 5) assert.ok(Math.abs(f.dots[k]! - mid) >= SHORT_OF_MIDDLE - 1e-9, `nothing at the middle of the gap after ${parts[j]!.stand} at ${t}s`);
    });
  }
});

test("each stretch is its own ring's colours: needs you warm, working all round the prism", () => {
  const colours = (s: Shown) => {
    const f = frame(stretches([s], [1], 300), 300, 1.3);
    const out: [number, number, number][] = [];
    for (let k = 0; k < f.dots.length; k += 5) {
      const c = f.dots[k + 4]!;
      out.push([(c >> 16) & 255, (c >> 8) & 255, c & 255]);
    }
    return out;
  };
  assert.ok(colours("needs").every(([r, , b]) => r > b), "needs you's gold");
  const working = colours("working");
  assert.ok(working.some(([r, , b]) => b > r + 20), "working's blues");
  assert.ok(working.some(([r, , b]) => r > b + 20), "and its pinks and golds");
});

test("a frame of every state on a desktop's rule is quick to make", () => {
  const parts = stretches(ALL, COUNTS, 920);
  const begun = performance.now();
  for (let t = 0; t < 1; t += 0.1) frame(parts, 920, t);
  const each = (performance.now() - begun) / 10;
  assert.ok(each < 40, `${each.toFixed(1)} ms a frame`);
});

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

test("a stretch coming or going grows from nothing, and one gap with it, so its neighbours end one gap apart", () => {
  const parts = stretches(["needs", "working", "finished"], [3, 0.5, 9], W);
  const half = parts[1]!.x1 - parts[1]!.x0;
  const whole = stretches(["needs", "working", "finished"], [3, 1, 9], W);
  assert.ok(half > 0 && half < whole[1]!.x1 - whole[1]!.x0, "half there: shorter than with its agent there whole");
  assert.ok(near(parts[1]!.x0 - parts[0]!.x1, 2.5) && near(parts[2]!.x0 - parts[1]!.x1, 5), "the gap before it half open, the one after whole");
  const gone = stretches(["needs", "working", "finished"], [3, 0, 9], W);
  assert.ok(near(gone[1]!.x1 - gone[1]!.x0, 0) && near(gone[2]!.x0 - gone[0]!.x1, 5), "gone: needs you and finished one gap apart");
  const without = stretches(["needs", "finished"], [3, 9], W);
  assert.ok(near(without[1]!.x0, gone[2]!.x0, 1e-6), "just where they are once it's left out");
  // The first stretch going: no gap is left before the next.
  const first = stretches(["needs", "working"], [0, 9], W);
  assert.ok(near(first[1]!.x0, 0), "the next starts at the rule's start");
});
