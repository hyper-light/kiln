/** Variants are told apart by a swatch and a name, never a letter; their weights stay at 100 (views/harness/variants.ts). */
import assert from "node:assert/strict";
import { test } from "node:test";
import { rebalance, variantName, variantTone, type Deployment } from "../src/views/harness/variants";

const G = 2 ** 30;
const v = (agentfile: string, cpus: number, memory: number, name: string | null = null): Deployment => ({ agentfile, engine: "microvm", cpus, memory: memory * G, name });

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
