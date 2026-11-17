/** Variants are told apart by a swatch and a name, never a letter; their weights stay at 100 (views/harness/variants.ts). */
import assert from "node:assert/strict";
import { test } from "node:test";
import type { Interval } from "../src/api/agentdefs";
import { measureAxis, onAxis, rebalance, rescale, variantName, variantTone, winsMeasure, type Deployment } from "../src/views/harness/variants";

const G = 2 ** 30;
const v = (agentfile: string, cpus: number, memory: number, name: string | null = null): Deployment => ({ agentfile, engine: "microvm", cpus, memory: memory * G, name });
/** On the host, as the server answers one (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d P7.1): no Agentfile, no size. */
const host = (name: string | null = null): Deployment => ({ agentfile: null, engine: "host", name });

test("a variant's default name is what sets it apart from the others", () => {
  const two = [v("rust-dev", 2, 4), v("rust-dev", 4, 8)];
  assert.deepEqual([variantName(two, 0), variantName(two, 1)], ["2 vCPU · 4 GiB", "4 vCPU · 8 GiB"]);
  const six = [v("rust-dev", 1, 2), v("rust-dev", 2, 2), v("rust-dev", 2, 4), v("rust-dev", 4, 4), v("rust-dev", 4, 8), v("docs-site", 2, 4)];
  assert.deepEqual(
    six.map((_, i) => variantName(six, i)),
    ["rust-dev · 1 vCPU · 2 GiB", "rust-dev · 2 vCPU · 2 GiB", "rust-dev · 2 vCPU · 4 GiB", "rust-dev · 4 vCPU · 4 GiB", "rust-dev · 4 vCPU · 8 GiB", "docs-site · 2 vCPU · 4 GiB"],
  );
  // Memory alone differs: only memory names them.
  const mem = [v("rust-dev", 2, 4), v("rust-dev", 2, 8)];
  assert.deepEqual([variantName(mem, 0), variantName(mem, 1)], ["4 GiB", "8 GiB"]);
});

test("a name a person gave holds, and alike variants are told apart by their place, with no letter anywhere", () => {
  const vs = [v("rust-dev", 2, 4, "small"), v("rust-dev", 2, 4), v("rust-dev", 2, 4)];
  const names = vs.map((_, i) => variantName(vs, i));
  assert.deepEqual(names, ["small", "2 vCPU · 4 GiB", "2 vCPU · 4 GiB (2)"]);
  for (const n of names) assert.doesNotMatch(n, /^[A-Z]{1,2}$/);
});

test("p7_1 a variant on the host is the host, with no Agentfile or size to say, and an Agentfile is said by its name", () => {
  const beside = [v("harness-claude", 2, 4), host()];
  assert.deepEqual([variantName(beside, 0), variantName(beside, 1)], ["microVM", "host"]);
  const three = [v("harness-claude", 2, 4), v("harness-claude", 4, 8), host()];
  assert.deepEqual(
    three.map((_, i) => variantName(three, i)),
    ["microVM · 2 vCPU · 4 GiB", "microVM · 4 vCPU · 8 GiB", "host"],
  );
  const hosts = [host(), host()];
  assert.deepEqual([variantName(hosts, 0), variantName(hosts, 1)], ["host", "host (2)"]);
  // The server names one of yours by its id; its name is what's said, the id where it isn't known.
  const names = new Map([["agentfile:aa", "rust-dev"]]);
  const yours = [v("agentfile:aa", 2, 4), v("agentfile:bb", 2, 4)];
  assert.deepEqual([variantName(yours, 0, names), variantName(yours, 1, names)], ["rust-dev", "agentfile:bb"]);
});

test("p7_1 a removed variant's share goes back to the others in proportion, each whole, as kiln scales it", () => {
  assert.deepEqual(rescale([30, 30]), [50, 50]);
  // The largest remainders get what's left, the earliest first among equals.
  assert.deepEqual(rescale([20, 20, 20, 15, 15]), [22, 22, 22, 17, 17]);
  assert.deepEqual(rescale([0, 50, 25]), [0, 67, 33]);
  assert.equal(rescale([0, 0]), null);
});

test("swatches run along kiln's prism from its first stop to its last, as many as there are variants", () => {
  assert.equal(variantTone(0, 2), "rgb(184 217 210)");
  assert.equal(variantTone(1, 2), "rgb(216 203 176)");
  const thirty = Array.from({ length: 30 }, (_, i) => variantTone(i, 30));
  assert.equal(new Set(thirty).size, 30);
});

test("moving one weight moves the others in proportion, always to 100", () => {
  assert.deepEqual(rebalance([50, 50], 0, 70), [70, 30]);
  const six = rebalance([20, 20, 20, 15, 15, 10], 2, 40);
  assert.equal(
    six.reduce((a, x) => a + x, 0),
    100,
  );
  assert.equal(six[2], 40);
});

/** A variant's measures, each [low, mid, high] (kiln plan 01a1dd2ea7a777a28970a43fc5f766d0 P7.2); `{}` has none. */
const measured = (warm: Interval, peak: Interval, failed: Interval) => ({ measures: { warm_ms: warm, peak_gib: peak, failed_pct: failed } });
const small = measured([208, 215, 223], [2.24, 2.29, 2.35], [6, 12, 20]);
const large = measured([212, 221, 229], [2.94, 3.0, 3.07], [0, 2, 8]);

test("p7_2 a measure's axis runs from the lowest low to the highest high of its variants' intervals, each drawn along it", () => {
  assert.deepEqual(measureAxis([small, large, {}], "warm_ms"), [208, 229]);
  assert.deepEqual(measureAxis([small, large], "failed_pct"), [0, 20]);
  assert.equal(measureAxis([{}, {}], "warm_ms"), null);
  assert.deepEqual([onAxis(208, [208, 229]), onAxis(229, [208, 229]), onAxis(5, [0, 20])], [0, 100, 25]);
  // An axis of one value: its middle.
  assert.equal(onAxis(3, [3, 3]), 50);
});

test("p7_2 a variant wins a measure only where its whole interval is below every other's, and none does beside one without measures", () => {
  // Peak memory: Small's high under Large's low. Warm starts and failures overlap: neither wins.
  assert.deepEqual([winsMeasure([small, large], 0, "peak_gib"), winsMeasure([small, large], 1, "peak_gib")], [true, false]);
  assert.deepEqual([winsMeasure([small, large], 0, "warm_ms"), winsMeasure([small, large], 1, "warm_ms")], [false, false]);
  assert.deepEqual([winsMeasure([small, large], 0, "failed_pct"), winsMeasure([small, large], 1, "failed_pct")], [false, false]);
  // Intervals that touch overlap.
  const touching = [measured([1, 2, 3], [1, 1, 1], [0, 0, 0]), measured([3, 4, 5], [2, 2, 2], [1, 1, 1])];
  assert.equal(winsMeasure(touching, 0, "warm_ms"), false);
  // A variant with no measures (a host's, or one too new to bound them) leaves each measure without a winner.
  assert.equal(winsMeasure([small, large, {}], 0, "peak_gib"), false);
  assert.equal(winsMeasure([small, {}], 1, "peak_gib"), false);
});

test("p7_2 a measure some variants lack has the axis of those that have it, and no winner", () => {
  // A host's sessions: failures alone, no warm start or peak.
  const host = { measures: { failed_pct: [0, 1, 9] as Interval } };
  assert.deepEqual(measureAxis([small, host], "warm_ms"), [208, 223]);
  assert.deepEqual(measureAxis([small, host], "failed_pct"), [0, 20]);
  assert.deepEqual([winsMeasure([small, host], 0, "warm_ms"), winsMeasure([small, host], 1, "warm_ms")], [false, false]);
  // Failures, which both have: [0, 9] overlaps [6, 20]; [0, 4] clears it.
  assert.equal(winsMeasure([small, host], 1, "failed_pct"), false);
  assert.equal(winsMeasure([small, { measures: { failed_pct: [0, 1, 4] as Interval } }], 1, "failed_pct"), true);
});
