import assert from "node:assert/strict";
import { test } from "node:test";
import { keptAt, noPoolsLine, poolAgent, poolHits, poolSize, poolUntil } from "../src/views/warm-pools.ts";

test("P3.2: a runner's warm pool says its agent as you may know it, and nothing of another person's", () => {
  assert.equal(poolAgent({ agent_from: "definition", agent: "01a1def", agent_name: "reviewer", variant: null }), "reviewer");
  assert.equal(poolAgent({ agent_from: "harness", agent: "claude", agent_name: "Claude Code", variant: null }), "Claude Code");
  assert.equal(poolAgent({ agent_from: "definition", agent: "01a1def", agent_name: "reviewer", variant: { experiment: "faster-start", arm: "b" } }), "reviewer · b of faster-start");
  assert.equal(poolAgent({ agent_from: "another", agent: null, agent_name: null, variant: null }), "Another person's agent");
});

test("P3.2: a runner's warm pool says its size, its hits and when it's let go", () => {
  assert.equal(poolSize({ vcpus: 2, memory_mib: 2048 }), "2 vCPUs · 2.0 GiB");
  assert.equal(poolSize({ vcpus: 1, memory_mib: 0 }), "1 vCPU");
  assert.equal(poolSize({ vcpus: null, memory_mib: null }), "least size");
  assert.equal(poolHits({ hits: 1, starts: 2 }), "1 hit · 2 starts");
  assert.equal(poolHits({ hits: 0, starts: 1 }), "0 hits · 1 start");
  assert.equal(poolUntil({ until_ms: 600_000 }, 0), "let go in 10 min");
  assert.equal(poolUntil({ until_ms: 30_000 }, 0), "let go within a minute");
  assert.equal(noPoolsLine(600_000), "None now. An agent started in a microVM here names its pool, kept 10 minutes past its latest start.");
});

test("P3.2: a pool whose keep window has passed is gone from the list, kept through its window's last ms", () => {
  const pools = [{ until_ms: 1_000 }, { until_ms: 2_000 }];
  assert.deepEqual(keptAt(pools, 1_000), pools);
  assert.deepEqual(keptAt(pools, 1_001), [{ until_ms: 2_000 }]);
  assert.deepEqual(keptAt(pools, 2_001), []);
});
