import assert from "node:assert/strict";
import { test } from "node:test";
import { beads, cells, drawn, LIVELINESS, LOOK, meets, reaches, stretches, wave, Y, type Shown } from "../src/views/harness/stands-wave.ts";

// Mission Control's status rule (stands-wave.ts): its geometry, as stands.tsx draws it each frame.

const ALL: Shown[] = ["needs", "crashed", "paused", "working", "finished", "ready", "offline"];
const COUNTS = [2, 1, 1, 6, 14, 2, 1];
const W = 390;
const near = (a: number, b: number, eps = 1e-9) => Math.abs(a - b) <= eps;

test("the stretches fill the rule in order, each its share of the agents, a gap between each two", () => {
  const parts = stretches(ALL, COUNTS, W);
  assert.deepEqual(parts.map((p) => p.stand), ALL);
  assert.ok(near(parts[0].x0, 0) && near(parts[parts.length - 1].x1, W, 1e-6), "from end to end");
  parts.slice(1).forEach((p, i) => assert.ok(near(p.x0 - parts[i].x1, 5, 1e-9), `a gap of 5 after ${parts[i].stand}`));
  for (const p of parts) assert.ok(p.x1 - p.x0 >= 12 - 1e-9, `${p.stand} at least 12 long`);
  const len = (s: Shown) => parts.find((p) => p.stand === s)!.x1 - parts.find((p) => p.stand === s)!.x0;
  assert.ok(len("finished") > len("working") && len("working") > len("needs") && near(len("crashed"), len("paused")), "longer for more agents");
});

test("one wave runs across every stretch and gap unbroken, held at the line at the rule's ends", () => {
  const parts = stretches(ALL, COUNTS, W);
  const h = reaches(parts);
  for (const t of [0, 0.7, 3.3, 7.9]) {
    assert.ok(near(wave(parts, h, W, 0, t), Y) && near(wave(parts, h, W, W, t), Y), `held at its ends at ${t}s`);
    let worst = 0;
    for (let x = 0; x < W; x += 0.5) worst = Math.max(worst, Math.abs(wave(parts, h, W, x + 0.5, t) - wave(parts, h, W, x, t)));
    // Working's waves are the steepest: 2π·2.9/18 + 2π·1.05/31 ≈ 1.23 a point, so 0.62 a half point.
    assert.ok(worst < 0.65, `no break at ${t}s: its steepest half-point step is ${worst.toFixed(3)}`);
  }
});

test("each stretch is as lively as its agents: working the most, then ready, needs you, paused, finished, stopped; offline lies flat", () => {
  const strength = (s: Shown) => LOOK[s].waves.reduce((sum, [amp]) => sum + amp, 0);
  LIVELINESS.slice(1).forEach((s, i) => assert.ok(strength(s) < strength(LIVELINESS[i]), `${s} stiller than ${LIVELINESS[i]}`));
  assert.equal(strength("offline"), 0);
  // Drawn: alone across the rule, how far its middle rises or falls over a few seconds.
  const swing = (s: Shown) => {
    const parts = stretches([s], [1], 300);
    const h = reaches(parts);
    let most = 0;
    for (let t = 0; t < 8; t += 0.1) for (let x = 100; x <= 200; x += 1) most = Math.max(most, Math.abs(wave(parts, h, 300, x, t) - Y));
    return most;
  };
  assert.ok(swing("working") > swing("ready") && swing("ready") > swing("finished") && swing("finished") > swing("offline"), "drawn so too");
  assert.equal(swing("offline"), 0);
});

test("at each gap both stretches reach for the same point, stretching toward each other, never touching", () => {
  const parts = stretches(ALL, COUNTS, W);
  const h = reaches(parts);
  for (const t of [0, 1.35, 4.2]) {
    const m = meets(parts, h, W, t);
    m.forEach((meet, j) => {
      assert.ok(near(meet.x, (parts[j].x1 + parts[j + 1].x0) / 2), "the meeting point is the gap's middle");
      assert.ok(near(drawn(parts, h, W, j, parts[j].x1, t, m), meet.y), `${parts[j].stand}'s end reaches its height at ${t}s`);
      assert.ok(near(drawn(parts, h, W, j + 1, parts[j + 1].x0, t, m), meet.y), `${parts[j + 1].stand}'s start reaches it too`);
      // Each end stretches at most 1.15 into the 5-point gap: with their round caps (1 each),
      // what's left between them is never under 0.7.
      assert.ok(meet.e >= 0 && meet.e <= 1.15 + 1e-9 && 5 - 2 * meet.e - 2 >= 0.7 - 1e-9, `the ends never touch (${meet.e})`);
    });
    // Beads reach too: paused's first and last beads sit at the heights where they meet.
    const i = parts.findIndex((p) => p.stand === "paused");
    const b = beads(parts, h, W, i, t, m, 5, 1.1);
    assert.ok(b.length >= 1);
    assert.ok(b[0][0] <= parts[i].x0 + 5 && b[b.length - 1][0] >= parts[i].x1 - 5, "the beads span their stretch");
  }
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
  const half = parts[1].x1 - parts[1].x0;
  const whole = stretches(["needs", "working", "finished"], [3, 1, 9], W);
  assert.ok(half > 0 && half < whole[1].x1 - whole[1].x0, "half there: shorter than with its agent there whole");
  assert.ok(near(parts[1].x0 - parts[0].x1, 2.5) && near(parts[2].x0 - parts[1].x1, 5), "the gap before it half open, the one after whole");
  const gone = stretches(["needs", "working", "finished"], [3, 0, 9], W);
  assert.ok(near(gone[1].x1 - gone[1].x0, 0) && near(gone[2].x0 - gone[0].x1, 5), "gone: needs you and finished one gap apart");
  const without = stretches(["needs", "finished"], [3, 9], W);
  assert.ok(near(without[1].x0, gone[2].x0, 1e-6), "just where they are once it's left out");
  // The first stretch going: no gap is left before the next.
  const first = stretches(["needs", "working"], [0, 9], W);
  assert.ok(near(first[1].x0, 0), "the next starts at the rule's start");
});
