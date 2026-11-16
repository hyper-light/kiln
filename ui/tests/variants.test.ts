/** Variants are told apart by a swatch and a name, never a letter; their weights stay at 100 (views/harness/variants.ts). */
import assert from "node:assert/strict";
import { test } from "node:test";
import { rebalance, rescale, variantName, variantTone, type Deployment } from "../src/views/harness/variants";

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
