import assert from "node:assert/strict";
import { test } from "node:test";
import { condenseFrame, HEIGHT, lineAt, prismAt, Y } from "../src/views/harness/condense-light.ts";

// A compaction under way (condense-light.ts): a thread of the prism along the line, and grains
// either side drifting in to it.

const W = 390;
const TIMES = [0, 0.9, 2.6, 7.3, 31.1];

test("the line is held at its ends and only breathes", () => {
  for (const t of TIMES) {
    assert.ok(Math.abs(lineAt(W, 0, t)) < 1e-9 && Math.abs(lineAt(W, W, t)) < 1e-9, `held at ${t}s`);
    for (let x = 0; x <= W; x += 3) assert.ok(Math.abs(lineAt(W, x, t)) < 0.6, "under 0.6 points");
  }
});

test("most grains lie on the thread, some come in from either side, all in the box and none at the line's very ends", () => {
  for (const t of TIMES) {
    const f = condenseFrame(W, t);
    let on = 0;
    let above = 0;
    let below = 0;
    for (let k = 0; k < f.dots.length; k += 5) {
      const [x, y, r] = [f.dots[k]!, f.dots[k + 1]!, f.dots[k + 2]!];
      const off = y - Y - lineAt(W, x, t);
      if (Math.abs(off) < 2) on++;
      if (off < -4) above++;
      if (off > 4) below++;
      assert.ok(y - r >= 0 && y + r <= HEIGHT, `y ${y.toFixed(2)} in the box`);
      assert.ok(x > 0.5 && x < W - 0.5, `x ${x.toFixed(2)} off the ends`);
    }
    const n = f.dots.length / 5;
    assert.ok(on > 0.6 * n, `${on} of ${n} on the thread at ${t}s`);
    assert.ok(above > 0.015 * n && below > 0.015 * n, `coming in from above (${above}) and below (${below}) at ${t}s`);
  }
});

test("a grain coming in nears the line as it goes, and fades into it", () => {
  // Far from the line, a moment later: fewer the further out, and brighter close in than far out.
  const f = condenseFrame(W, 5.2);
  const bands: number[][] = [[], [], []];
  for (let k = 0; k < f.dots.length; k += 5) {
    const off = Math.abs(f.dots[k + 1]! - Y - lineAt(W, f.dots[k]!, 5.2));
    if (off > 2) bands[off < 6 ? 0 : off < 10 ? 1 : 2]!.push(f.dots[k + 3]!);
  }
  assert.ok(bands[0]!.length > bands[2]!.length, `more near (${bands[0]!.length}) than far (${bands[2]!.length}): they slow as they come`);
});

test("the thread wears the prism the summary's bar will, from its start to its end", () => {
  const rgb = (c: number) => [(c >> 16) & 255, (c >> 8) & 255, c & 255] as const;
  const [r0, g0] = rgb(prismAt(0));
  const [r1, , b1] = rgb(prismAt(1));
  assert.ok(g0 > r0, "its start the prism's green");
  assert.ok(r1 > b1, "its end the prism's sand");
  const f = condenseFrame(W, 3);
  const mean = (lo: number, hi: number) => {
    const cs: (readonly [number, number, number])[] = [];
    for (let k = 0; k < f.dots.length; k += 5) if (f.dots[k]! >= lo && f.dots[k]! < hi) cs.push(rgb(f.dots[k + 4]!));
    return [0, 1, 2].map((i) => cs.reduce((s, c) => s + c[i]!, 0) / cs.length);
  };
  const start = mean(20, 60);
  const end = mean(W - 60, W - 20);
  assert.ok(start[1]! > start[0]! && end[0]! > end[2]!, `green at the start (${start.map(Math.round)}), sand at the end (${end.map(Math.round)})`);
});

test("a frame across a desktop's conversation is quick to make", () => {
  const begun = performance.now();
  for (let t = 0; t < 1; t += 0.1) condenseFrame(920, t);
  const each = (performance.now() - begun) / 10;
  assert.ok(each < 15, `${each.toFixed(1)} ms a frame`);
});
