import assert from "node:assert/strict";
import { test } from "node:test";
import { cells, COMPACTING, frame, HEIGHT, LIVE, LIVELINESS, MEDIUM, reachOf, SHORT_OF_MIDDLE, stretches, stringOf, type Shown } from "../src/views/harness/stands-light.ts";

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

test("one string runs the whole rule unbroken, held at its two ends, and never comes round the same", () => {
  const parts = stretches(ALL, COUNTS, W);
  for (const t of [...TIMES, 13.6, 21.2, 37.5]) {
    const s = stringOf(parts, W, t);
    assert.ok(near(s.at(0), 0) && near(s.at(W), 0, 1e-6), `held at its ends at ${t}s`);
    // Its steepest wave and the ripples on it together, gaps and all, stay well under half a
    // point of rise in a quarter point: no break anywhere.
    let worst = 0;
    for (let x = 0; x < W; x += 0.25) worst = Math.max(worst, Math.abs(s.at(x + 0.25) - s.at(x)));
    assert.ok(worst < 0.5, `no break at ${t}s: its steepest quarter-point step is ${worst.toFixed(3)}`);
    // Its waves' periods wander and its ripples come at random: 8 s on, and 16, it's elsewhere.
    for (const on of [8, 16]) {
      const later = stringOf(parts, W, t + on);
      let apart = 0;
      for (let x = 0; x <= W; x += 3) apart = Math.max(apart, Math.abs(s.at(x) - later.at(x)));
      assert.ok(apart > 0.2, `${on} s after ${t}s it isn't where it was (${apart.toFixed(3)} at most)`);
    }
  }
});

test("a ripple runs through each stretch as its medium has it, quicker the livelier", () => {
  assert.deepEqual([...LIVELINESS].sort((a, b) => MEDIUM[b] - MEDIUM[a]), [...LIVELINESS]);
  const parts = stretches(ALL, COUNTS, W);
  const s = stringOf(parts, W, 0);
  // Through the middle of each stretch, away from its gaps' easing, it takes a point 1/MEDIUM seconds.
  for (const p of parts) {
    if (p.x1 - p.x0 < 30) continue;
    const a = p.x0 + 13;
    const b = p.x1 - 13;
    const each = (s.arrival(b) - s.arrival(a)) / (b - a);
    assert.ok(Math.abs(each * MEDIUM[p.stand] - 1) < 0.02, `${p.stand}: ${(1 / each).toFixed(1)} points a second, its medium ${MEDIUM[p.stand]}`);
  }
  // From end to end, it takes the stretches' lengths through their media, and the gaps' (each
  // through its neighbours' media, somewhere between the two).
  const through = parts.reduce((sum, p) => sum + (p.x1 - p.x0) / MEDIUM[p.stand], 0);
  const gaps = parts.slice(1).map((p, j) => [(p.x0 - parts[j]!.x1) / Math.max(MEDIUM[p.stand], MEDIUM[parts[j]!.stand]), (p.x0 - parts[j]!.x1) / Math.min(MEDIUM[p.stand], MEDIUM[parts[j]!.stand])] as const);
  const [least, most] = gaps.reduce(([a, b], [g0, g1]) => [a + g0, b + g1], [0, 0]);
  assert.ok(s.arrival(W) > (through + least) * 0.97 && s.arrival(W) < (through + most) * 1.03, `${s.arrival(W).toFixed(2)} s across, through the stretches ${through.toFixed(2)} s and the gaps ${least.toFixed(2)} to ${most.toFixed(2)} s`);
});

test("however hard it's struck, the string keeps to its box", () => {
  const parts = stretches(["working", "ready", "needs"], [3, 2, 1], W);
  let most = 0;
  for (let t = 0; t < 200; t += 0.2) {
    const s = stringOf(parts, W, t);
    for (let x = 0; x <= W; x += 4) most = Math.max(most, Math.abs(s.at(x)));
  }
  assert.ok(most < 9.5 && most > 2, `its swings stay in the box, the hardest ${most.toFixed(2)} points`);
});

test("each state damps the string as much as it's still: working the least, then ready, needs you, paused, finished, stopped; offline holds it still", () => {
  assert.deepEqual([...LIVELINESS].sort((a, b) => LIVE[b] - LIVE[a]), [...LIVELINESS]);
  // Alone across the rule: how far its middle swings, and how fast, over 30 s of its weather and ripples.
  const swing = (s: Shown) => {
    const parts = stretches([s], [1], 300);
    let most = 0;
    let fastest = 0;
    for (let t = 0; t < 30; t += 0.05) {
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

test("every particle stays in the rule's box, most of them fine points and a few larger, each seen", () => {
  const parts = stretches(ALL, COUNTS, W);
  const radii: number[] = [];
  for (const t of TIMES) {
    const f = frame(parts, W, t);
    assert.ok(f.dots.length > 5 * 500, "a fine, close line");
    for (let k = 0; k < f.dots.length; k += 5) {
      const [x, y, r, a] = [f.dots[k]!, f.dots[k + 1]!, f.dots[k + 2]!, f.dots[k + 3]!];
      assert.ok(x >= -0.5 && x <= W + 0.5, `x ${x} on the rule`);
      assert.ok(y - r >= 0 && y + r <= HEIGHT, `y ${y.toFixed(2)} in the box`);
      assert.ok(r > 0 && r < 2.5 && a > 0 && a <= 1, `a point, seen: r ${r.toFixed(2)}, a ${a.toFixed(2)}`);
      radii.push(r);
    }
    for (let k = 0; k < f.soft.length; k += 6) assert.ok(f.soft[k + 1]! >= 0 && f.soft[k + 1]! <= HEIGHT, "soft light about the string");
  }
  radii.sort((a, b) => a - b);
  const p95 = radii[Math.floor(0.95 * (radii.length - 1))]!;
  assert.ok(p95 < 0.8, `most are fine points: 95 in 100 under ${p95.toFixed(2)} points`);
  assert.ok(radii[radii.length - 1]! > 1, "and a few are larger");
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

test("a compaction's ribbon is working's light about three times as wide, turning face-on and edge-on along it, as many particles, all in the box", () => {
  const parts = stretches(["working"], [1], W);
  // How far most of each 10 points' fine points span across the string (a tenth either side left
  // out), the tapered ends aside.
  const spans = (f: ReturnType<typeof frame>) => {
    const bins: number[][] = Array.from({ length: Math.floor(W / 10) }, () => []);
    for (let k = 0; k < f.dots.length; k += 5) bins[Math.floor(f.dots[k]! / 10)]?.push(f.dots[k + 1]!);
    return bins.slice(2, -2).map((ys) => {
      ys.sort((a, b) => a - b);
      return ys[Math.floor(0.9 * (ys.length - 1))]! - ys[Math.floor(0.1 * (ys.length - 1))]!;
    });
  };
  const count = (f: ReturnType<typeof frame>) => f.dots.length / 5 + f.soft.length / 6;
  for (const t of TIMES) {
    const rule = frame(parts, W, t);
    const ribbon = frame(parts, W, t, undefined, COMPACTING);
    assert.ok(Math.abs(count(ribbon) - count(rule)) <= 0.01 * count(rule), `the rule's cost at ${t}s: ${count(ribbon)} particles to its ${count(rule)}`);
    for (let k = 0; k < ribbon.dots.length; k += 5) {
      const [y, r] = [ribbon.dots[k + 1]!, ribbon.dots[k + 2]!];
      assert.ok(y - r >= 0 && y + r <= HEIGHT, `y ${y.toFixed(2)} in the box at ${t}s`);
    }
    for (let k = 0; k < ribbon.soft.length; k += 6) assert.ok(ribbon.soft[k + 1]! >= 0 && ribbon.soft[k + 1]! <= HEIGHT, `soft light in the box at ${t}s`);
    const wide = Math.max(...spans(ribbon));
    const rules = Math.max(...spans(rule));
    assert.ok(wide > 2.2 * rules, `face-on ${wide.toFixed(1)} points across at ${t}s, the rule's ${rules.toFixed(1)}`);
    assert.ok(Math.min(...spans(ribbon)) < wide / 4, `edge-on a knot at ${t}s: ${Math.min(...spans(ribbon)).toFixed(1)} points`);
  }
  const desktop = stretches(["working"], [1], 920);
  const begun = performance.now();
  for (let t = 0; t < 1; t += 0.1) frame(desktop, 920, t, undefined, COMPACTING);
  const each = (performance.now() - begun) / 10;
  assert.ok(each < 40, `${each.toFixed(1)} ms a frame across a desktop's conversation`);
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
