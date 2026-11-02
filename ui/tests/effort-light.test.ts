import assert from "node:assert/strict";
import { test } from "node:test";
import { clocksAt, effortFrame, effortString, energy, HEIGHT, tickClocks, Y } from "../src/views/harness/effort-light.ts";

// The effort scale's lit line (effort-light.ts): a string of light from the scale's start to its
// knob that tapers out along it, a calm thread at the start and the more of a swarm the further
// along, so the higher the notch the further and wilder it runs, as run-settings.tsx draws it.

const FILL = 300;
/** The track the notches are on: at each, the line runs from its start to the knob. */
const TRACK = 300;
const NOTCHES = [0, 0.25, 0.5, 0.75, 1];
const TIMES = [0.4, 1.7, 3.1, 5.9, 8.3, 12.6];
const sum = (xs: number[]) => xs.reduce((s, x) => s + x, 0);

/** Over several moments, from `x0` to `x1` of a line `fill` long at `level`: how many particles, how much light (each one's opacity, added up), how far the swarm reaches from the line (its 95th percentile), and how far the string moves in 50 ms (a point, on average). */
function measure(fill: number, level: number, x0 = 0, x1 = fill) {
  const count: number[] = [];
  const light: number[] = [];
  const reach: number[] = [];
  const moves: number[] = [];
  const on = (x: number) => x >= x0 && x < x1;
  for (const t of TIMES) {
    const f = effortFrame(fill, level, t);
    let n = 0;
    const alphas: number[] = [];
    const offsets: number[] = [];
    for (let k = 0; k + 4 < f.dots.length; k += 5) {
      if (!on(f.dots[k] ?? -1)) continue;
      n++;
      alphas.push(f.dots[k + 3] ?? 0);
      offsets.push(Math.abs((f.dots[k + 1] ?? Y) - Y));
    }
    for (let k = 0; k + 5 < f.soft.length; k += 6) if (on(f.soft[k] ?? -1)) n++;
    count.push(n);
    light.push(sum(alphas));
    offsets.sort((a, b) => a - b);
    reach.push(offsets[Math.floor(0.95 * (offsets.length - 1))] ?? 0);
    const a = effortString(fill, t, level);
    const b = effortString(fill, t + 0.05, level);
    const moved: number[] = [];
    for (let x = x0; x < x1; x += 2) moved.push(Math.abs(b.at(x) - a.at(x)));
    moves.push(sum(moved) / Math.max(1, moved.length));
  }
  const mean = (xs: number[]) => sum(xs) / xs.length;
  return { count: mean(count), light: mean(light), reach: mean(reach), moves: mean(moves) };
}

test("it tapers out: from a calm thread at the start, each stretch further along has more particles, more light, a wider swarm and a livelier string", () => {
  const q = [0, 1, 2, 3].map((i) => measure(TRACK, 1, (i * TRACK) / 4, ((i + 1) * TRACK) / 4));
  for (let i = 1; i < q.length; i++) {
    const [a, b] = [q[i - 1]!, q[i]!];
    const at = `from quarter ${i} to ${i + 1}`;
    assert.ok(b.count > a.count * 1.2, `${at}: more particles (${a.count.toFixed(0)} to ${b.count.toFixed(0)})`);
    assert.ok(b.light > a.light * 1.2, `${at}: more light (${a.light.toFixed(0)} to ${b.light.toFixed(0)})`);
    assert.ok(b.reach > a.reach * 1.2, `${at}: the swarm reaches further (${a.reach.toFixed(2)} to ${b.reach.toFixed(2)} points)`);
    assert.ok(b.moves > a.moves * 1.2, `${at}: the string moves more (${a.moves.toFixed(2)} to ${b.moves.toFixed(2)})`);
  }
  assert.ok(q[3]!.count > 3 * q[0]!.count && q[3]!.reach > 3 * q[0]!.reach, "at the knob it swarms where at the start it barely stirs");
});

test("each notch up runs it further and wilder at its end", () => {
  const at = NOTCHES.slice(1).map((level) => ({ level, whole: measure(TRACK * level, level), end: measure(TRACK * level, level, 0.8 * TRACK * level) }));
  for (let i = 1; i < at.length; i++) {
    const [a, b] = [at[i - 1]!, at[i]!];
    const step = `from ${a.level} to ${b.level}`;
    assert.ok(b.whole.count > a.whole.count * 1.2, `${step}: more particles (${a.whole.count.toFixed(0)} to ${b.whole.count.toFixed(0)})`);
    assert.ok(b.whole.light > a.whole.light * 1.2, `${step}: more light (${a.whole.light.toFixed(0)} to ${b.whole.light.toFixed(0)})`);
    assert.ok(b.end.reach > a.end.reach * 1.2, `${step}: its end's swarm reaches further (${a.end.reach.toFixed(2)} to ${b.end.reach.toFixed(2)} points)`);
    assert.ok(b.end.moves > a.end.moves * 1.2, `${step}: its end's string moves more (${a.end.moves.toFixed(2)} to ${b.end.moves.toFixed(2)})`);
  }
  for (const k of ["flow", "spread", "waves", "ripples", "sparkle", "bright"] as const) assert.ok(energy(1)[k] > energy(0)[k], `${k} grows with effort`);
  assert.ok(energy(1).turn < energy(0).turn && energy(1).current < energy(0).current && energy(1).spark < energy(0).spark, "the ribbon, the colours and the glints go round faster");
});

test("a stretch of the track is as full whichever notch lights it", () => {
  // The second notch's whole line, and the first quarter of the last's.
  const short = measure(TRACK / 4, 0.25);
  const long = measure(TRACK, 1, 0, TRACK / 4);
  const near = (a: number, b: number) => a > 0.8 * b && a < 1.25 * b;
  assert.ok(near(short.count, long.count), `as many particles (${short.count.toFixed(0)} and ${long.count.toFixed(0)})`);
  assert.ok(near(short.light, long.light), `as much light (${short.light.toFixed(0)} and ${long.light.toFixed(0)})`);
});

/** The track's stops (points along it), and their grey where the line hasn't reached them. */
const STOPS = [0, 75, 150, 225, 300];
const GREY = 0x3a3c44;
/** A colour within `by` of the stops' grey in each channel. */
const greyish = (c: number, by = 40) => [16, 8, 0].every((s) => Math.abs(((c >> s) & 255) - ((GREY >> s) & 255)) <= by);
/** A stop past the knob at `x`: still the dot it is, stirring within bounds (lifted under 0.7 of a point, swollen a fifth at most), never as bright as a bead (no channel over 160: the brightest grey reaches 157, the dimmest bead 166). */
const stillGrey = (dot: number[] | null, x: number) =>
  dot !== null &&
  dot[0] === x &&
  Math.abs((dot[1] ?? 0) - Y) < 0.7 &&
  (dot[2] ?? 0) >= 2.5 &&
  (dot[2] ?? 0) <= 2.5 * 1.2 + 1e-9 &&
  dot[3] === 1 &&
  [16, 8, 0].every((s) => (((dot[4] ?? 0) >> s) & 255) <= 160);
/** The last crisp point and the last soft light at exactly `x` (the stops are drawn last), or none. */
function drawnAt(f: { dots: number[]; soft: number[] }, x: number) {
  let dot: number[] | null = null;
  let soft: number[] | null = null;
  for (let k = 0; k + 4 < f.dots.length; k += 5) if (f.dots[k] === x) dot = f.dots.slice(k, k + 5);
  for (let k = 0; k + 5 < f.soft.length; k += 6) if (f.soft[k] === x) soft = f.soft.slice(k, k + 6);
  return { dot, soft };
}

test("the stops: grey past the knob, and each the line has reached a bead of its light on the string, breathing, the larger and brighter the further along", () => {
  // At the fourth notch: its knob on the fourth stop, the first three reached, the last past it.
  const [fill, level] = [225, 0.75];
  const glow: number[][] = [[], [], []];
  const bright: number[][] = [[], [], []];
  for (const t of TIMES) {
    const f = effortFrame(fill, level, t, { stops: STOPS });
    const s = effortString(fill, t, level);
    assert.ok(stillGrey(drawnAt(f, 300).dot, 300), `past the knob, the grey dot (${t}s)`);
    for (const [i, x] of [0, 75, 150].entries()) {
      const { dot, soft } = drawnAt(f, x);
      assert.ok(dot && !greyish(dot[4] ?? GREY), `stop ${i} is a bead of light, not grey (${t}s)`);
      assert.ok(Math.abs((dot[1] ?? 0) - (Y + s.at(x))) < 1e-9, `stop ${i} rides the string (${t}s)`);
      assert.ok(soft, `stop ${i} glows (${t}s)`);
      glow[i]!.push(soft[2] ?? 0);
      bright[i]!.push(soft[3] ?? 0);
    }
  }
  const mean = (xs: number[]) => sum(xs) / xs.length;
  for (let i = 1; i < 3; i++) {
    assert.ok(mean(glow[i]!) > 1.2 * mean(glow[i - 1]!), `stop ${i} glows wider than stop ${i - 1} (${mean(glow[i - 1]!).toFixed(1)} to ${mean(glow[i]!).toFixed(1)} points)`);
    assert.ok(mean(bright[i]!) > mean(bright[i - 1]!), `stop ${i} glows brighter than stop ${i - 1}`);
  }
  for (const [i, sizes] of glow.entries())
    assert.ok(Math.max(...sizes) > 1.05 * Math.min(...sizes), `stop ${i}'s glow breathes (${Math.min(...sizes).toFixed(2)} to ${Math.max(...sizes).toFixed(2)})`);
});

test("as the line reaches a stop, its grey turns to light", () => {
  const before = drawnAt(effortFrame(140, 140 / 300, 2.5, { stops: STOPS }), 150);
  assert.ok(stillGrey(before.dot, 150), "not yet reached: grey");
  assert.ok(before.soft && (before.soft[3] ?? 1) <= 0.3, "not yet reached: a soft glow at most");
  const after = effortFrame(160, 160 / 300, 2.5, { stops: STOPS });
  const at = drawnAt(after, 150);
  assert.ok(at.dot && !greyish(at.dot[4] ?? GREY) && at.soft, "reached: a bead of light");
  let there = 0;
  for (let k = 0; k + 4 < after.dots.length; k += 5) if (after.dots[k] === 150) there++;
  assert.equal(there, 1, "reached: no grey left beside it");
  // Before the first notch's length: no line, the stops grey.
  const none = effortFrame(0, 0, 2.5, { stops: STOPS });
  for (const x of STOPS.slice(1)) assert.ok(stillGrey(drawnAt(none, x).dot, x), `before the first notch's length, the stop at ${x} is grey`);
});

/** Over 60 moments, the light (each particle's opacity, added up) between `x0` and `x1` of a line `fill` long at `level` on a track with stops `stops`, a point, and how far past the knob it lies, light for light; the stops' own dots and glows left out. */
function lightIn(fill: number, level: number, stops: readonly number[], x0: number, x1: number) {
  let light = 0;
  let far = 0;
  for (let k = 0; k < 60; k++) {
    const f = effortFrame(fill, level, 0.41 + k * 0.37, { stops });
    const add = (x: number, a: number) => {
      if (x < x0 || x >= x1 || stops.includes(x)) return;
      assert.ok(x <= (stops[stops.length - 1] ?? 0), `nothing past the track's end (${x.toFixed(2)})`);
      light += a;
      far += a * (x - fill);
    };
    for (let i = 0; i + 4 < f.dots.length; i += 5) add(f.dots[i] ?? 0, f.dots[i + 3] ?? 0);
    for (let i = 0; i + 5 < f.soft.length; i += 6) add(f.soft[i] ?? 0, f.soft[i + 3] ?? 0);
  }
  return { light: light / 60 / (x1 - x0), reach: far / Math.max(1e-9, light) };
}

test("past the knob the line spills on to the track: thinning away from the knob, never past the track's end, never fuller than the line", () => {
  for (const [fill, level] of [
    [0, 0],
    [75, 0.25],
    [150, 0.5],
    [225, 0.75],
  ] as const) {
    const near = lightIn(fill, level, STOPS, fill + 10, fill + 30);
    const further = lightIn(fill, level, STOPS, fill + 30, fill + 50);
    assert.ok(near.light > 0, `knob at ${fill}: it spills`);
    assert.ok(further.light < near.light, `knob at ${fill}: the further from the knob the thinner (${near.light.toFixed(3)} to ${further.light.toFixed(3)} a point)`);
    if (fill >= 40) {
      const line = lightIn(fill, level, STOPS, fill - 30, fill - 10);
      assert.ok(near.light < line.light, `knob at ${fill}: the spill is thinner than the line (${near.light.toFixed(3)} against ${line.light.toFixed(3)} a point)`);
    }
  }
  // At the fourth notch it pours past the knob: just past it, a third as full as the line just before it at least.
  const [line, spill] = [lightIn(225, 0.75, STOPS, 195, 215), lightIn(225, 0.75, STOPS, 235, 255)];
  assert.ok(spill.light > 0.3 * line.light, `a flow, not a trickle (${spill.light.toFixed(3)} against ${line.light.toFixed(3)} a point)`);
  // At the last notch the knob is at the track's end: nothing past it.
  lightIn(300, 1, STOPS, 0, 400);
});

test("the higher the notch, the more spills past the knob and the further", () => {
  // The knob in the same place on a longer track, its notch higher each time.
  const stops = [0, 150, 300, 450, 600];
  const spilled = [0.25, 0.5, 0.75].map((level) => ({ level, ...lightIn(150, level, stops, 151, 600) }));
  for (let i = 1; i < spilled.length; i++) {
    const [a, b] = [spilled[i - 1]!, spilled[i]!];
    assert.ok(b.light > 1.4 * a.light, `from ${a.level} to ${b.level}: more spills (${a.light.toFixed(3)} to ${b.light.toFixed(3)} a point)`);
    assert.ok(b.reach > a.reach, `from ${a.level} to ${b.level}: and further (${a.reach.toFixed(1)} to ${b.reach.toFixed(1)} points, light for light)`);
  }
});

test("past the knob the stops stir: the one nearest the knob the most, each further one less", () => {
  for (const [fill, level] of [
    [0, 0],
    [75, 0.25],
    [150, 0.5],
  ] as const) {
    const past = STOPS.filter((x) => x > fill);
    const lift = past.map(() => 0);
    const glow = past.map(() => 0);
    for (let k = 0; k < 80; k++) {
      const t = 0.37 + k * 0.29;
      const f = effortFrame(fill, level, t, { stops: STOPS });
      for (const [i, x] of past.entries()) {
        const { dot, soft } = drawnAt(f, x);
        assert.ok(stillGrey(dot, x), `knob at ${fill}: the stop at ${x} stays the grey dot (${t.toFixed(2)}s)`);
        assert.ok(soft && (soft[3] ?? 1) <= 0.3, `knob at ${fill}: the stop at ${x} glows softly (${t.toFixed(2)}s)`);
        lift[i] = Math.max(lift[i] ?? 0, Math.abs((dot?.[1] ?? Y) - Y));
        glow[i] = Math.max(glow[i] ?? 0, soft?.[3] ?? 0);
      }
    }
    assert.ok(lift[0]! > 0.3 && glow[0]! > 0.2, `knob at ${fill}: the stop nearest it stirs (${lift[0]!.toFixed(3)} points, glow ${glow[0]!.toFixed(2)} at most)`);
    for (let i = 1; i < past.length; i++) {
      assert.ok(lift[i]! < lift[i - 1]!, `knob at ${fill}: the stop at ${past[i]} stirs less than the one at ${past[i - 1]} (${lift[i - 1]!.toFixed(3)} to ${lift[i]!.toFixed(3)} points)`);
      assert.ok(glow[i]! < glow[i - 1]!, `knob at ${fill}: the stop at ${past[i]} glows less than the one at ${past[i - 1]}`);
    }
  }
});

test("the line is held at its start and at the knob, and its swarm keeps inside the canvas", () => {
  for (const level of NOTCHES) {
    for (const t of TIMES) {
      const s = effortString(FILL, t, level);
      assert.ok(Math.abs(s.at(0)) < 1e-9 && Math.abs(s.at(FILL)) < 1e-6, `held at both ends (level ${level}, ${t}s)`);
      const f = effortFrame(FILL, level, t, { kick: { ago: 0.2, size: 5.5 }, stops: STOPS });
      for (let k = 0; k + 4 < f.dots.length; k += 5) {
        const [x, y] = [f.dots[k] ?? 0, f.dots[k + 1] ?? 0];
        assert.ok(x >= 0 && x <= FILL, `a particle at x ${x.toFixed(2)} is on the line (level ${level}, ${t}s)`);
        assert.ok(y > 1 && y < HEIGHT - 1, `a particle at y ${y.toFixed(2)} keeps inside the canvas (level ${level}, ${t}s)`);
      }
      for (let k = 0; k + 5 < f.soft.length; k += 6) assert.ok((f.soft[k + 1] ?? 0) > 0 && (f.soft[k + 1] ?? 0) < HEIGHT, "its soft light too");
    }
  }
});

test("a step up kicks the line from the knob, and the kick runs back along it", () => {
  const level = 0.75;
  const t = 4.2;
  const ago = 0.15;
  const medium = energy(level).medium;
  const plain = effortString(FILL, t, level);
  const kicked = effortString(FILL, t, level, { ago, size: 1.5 + 4 * level });
  let nearKnob = 0;
  for (let x = FILL - 30; x < FILL - 4; x += 1) nearKnob = Math.max(nearKnob, Math.abs(kicked.at(x) - plain.at(x)));
  assert.ok(nearKnob > 0.5, `near the knob it's struck (${nearKnob.toFixed(2)} points at most)`);
  // It runs back at the line's medium: past where it could have reached, the line is as it was.
  const reached = FILL - ago * medium;
  for (let x = 0; x < reached - 2; x += 1) assert.ok(Math.abs(kicked.at(x) - plain.at(x)) < 1e-9, `not yet at ${x}`);
});

test("a step down's ripple settles the line: softer and slower than a step up's strike", () => {
  const [level, t, x] = [0.5, 4.2, FILL - 15];
  const swing = (down: boolean) => {
    let most = 0;
    let turns = 0;
    let was = 0;
    for (let ago = 0.02; ago < 1.6; ago += 0.02) {
      // As run-settings.tsx kicks it: up to this notch, or down to it from the one above.
      const kick = down ? { ago, size: 0.6 + 1.2 * 0.75, down } : { ago, size: 1 + 2.5 * level };
      const d = effortString(FILL, t + ago, level, kick).at(x) - effortString(FILL, t + ago, level).at(x);
      most = Math.max(most, Math.abs(d));
      if (Math.abs(d) > 0.05 && Math.sign(d) !== Math.sign(was)) turns++;
      if (Math.abs(d) > 0.05) was = d;
    }
    return { most, turns };
  };
  const [up, down] = [swing(false), swing(true)];
  assert.ok(down.most > 0.3, `a step down moves it too (${down.most.toFixed(2)} points)`);
  assert.ok(down.most < up.most, `softer (${down.most.toFixed(2)} against ${up.most.toFixed(2)} points)`);
  assert.ok(down.turns < up.turns, `slower (${down.turns} swings against ${up.turns})`);
});

test("a change of notch moves where the line ends, and nothing lit already: however long it has run", () => {
  // Long after the page opened, at the third notch and as the fourth comes in (its end still with
  // the third's knob, its clocks the same): the motes that float off the string (drawn soft, at the
  // very place they've drifted to) are where they were.
  const [fill, t] = [150, 1000.3];
  const clocks = clocksAt(t, 0.5);
  const at = (level: number) => {
    const f = effortFrame(fill, level, t, { clocks, stops: STOPS });
    const xs: number[] = [];
    for (let k = 0; k + 5 < f.soft.length; k += 6) if ((f.soft[k] ?? 0) < fill - 2) xs.push(f.soft[k] ?? 0);
    return xs;
  };
  const [a, b] = [new Set(at(0.5)), at(0.6)];
  const kept = b.filter((x) => a.has(x)).length / b.length;
  assert.ok(kept > 0.9, `the motes stay put (${(100 * kept).toFixed(0)}% exactly where they were)`);
  // The string moves by little more than the notch does, its ripples swelling or stilling rather than appearing.
  for (let k = 0; k < 20; k++) {
    const when = t + 0.37 * k;
    const [s, r] = [effortString(fill, when, 0.5, undefined, undefined, 300), effortString(fill, when, 0.51, undefined, undefined, 300)];
    let most = 0;
    for (let x = 0; x <= fill; x += 2) most = Math.max(most, Math.abs(s.at(x) - r.at(x)));
    assert.ok(most < 1, `at ${when.toFixed(2)}s the string is where it was, give or take (${most.toFixed(2)} points)`);
  }
});

test("the clocks count on at the notch's paces: faster after a step up, slower after a step down, never back", () => {
  const c = clocksAt(10, 0.5);
  const step = (level: number) => {
    const was = { ...c };
    tickClocks(c, 1 / 30, level);
    return [c.turn - was.turn, c.current - was.current, c.spark - was.spark];
  };
  const mid = step(0.5);
  const up = step(0.75);
  const down = step(0.25);
  for (const i of [0, 1, 2]) {
    assert.ok((mid[i] ?? 0) > 0 && (down[i] ?? 0) > 0, "each clock counts on");
    assert.ok((up[i] ?? 0) > (mid[i] ?? 0) && (down[i] ?? 0) < (mid[i] ?? 0), `clock ${i}: quicker after a step up, slower after a step down`);
  }
});

test("it never comes round the same, and a moment is drawn the same each time", () => {
  for (const level of [0.25, 1]) {
    for (const t of TIMES) {
      const now = effortString(FILL, t, level);
      for (const on of [8, 16]) {
        const later = effortString(FILL, t + on, level);
        let apart = 0;
        for (let x = 0; x <= FILL; x += 3) apart = Math.max(apart, Math.abs(now.at(x) - later.at(x)));
        assert.ok(apart > 0.05, `${on} s after ${t}s at level ${level} it isn't where it was (${apart.toFixed(3)} at most)`);
      }
    }
  }
  const a = effortFrame(FILL, 0.6, 2.5);
  const b = effortFrame(FILL, 0.6, 2.5);
  assert.deepEqual(a.dots, b.dots);
  assert.deepEqual(a.soft, b.soft);
  assert.equal(effortFrame(0.5, 1, 2.5).dots.length, 0, "no line before the first notch's length");
});
