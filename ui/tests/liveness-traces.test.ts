import assert from "node:assert/strict";
import { test } from "node:test";
import { trace, type Page } from "./liveness-traces.ts";

// bp-g9 step 2 (goal 9): the liveness rule on seeded traces of a 50 kbit/s link with a 6 s queue,
// 5% loss each way and 1 MB in flight (tests/liveness-traces.ts says the model). LIVENESS_TRACES
// sets how many of each (the plan's bar is 10,000; `npm test` runs 500). The same traces go through
// kiln-bp's Rust rule in crates/kiln-bp/tests/ui_liveness.rs, which must reach DIGEST too.

const N = Number(process.env.LIVENESS_TRACES ?? 500);
/** The verdicts of seeds 0..63 on shown pages, live and blackholed, folded (FNV-1a): Rust's must match. */
export const DIGEST = 2748312533;

const run = (page: Page, blackhole: boolean, n: number, from = blackhole ? 1_000_000 : 0) => Array.from({ length: n }, (_, i) => trace(from + i, { blackhole, page }));

test("bp_g9_s2 a blackholed link is found dead, never later than the rule's bound", () => {
  for (const page of ["shown", "hidden"] as const) {
    for (const o of run(page, true, N)) {
      assert.notEqual(o.foundAt, null, `${page}: found dead`);
      if (page === "shown") assert.ok(o.foundAt! <= o.bound!, `${page}: found at ${o.foundAt}, bound ${o.bound}`);
    }
  }
});

test("bp_g9_s2 a hidden tab whose timers run once a minute never reads a live link as dead", () => {
  assert.equal(
    run("hidden", false, N).reduce((n, o) => n + o.falseDead, 0),
    0,
  );
});

test("bp_g9_s2 the TypeScript rule reaches the Rust rule's verdicts on the same traces", () => {
  let d = 0x811c9dc5;
  for (const o of [...run("shown", false, 64), ...run("shown", true, 64)]) d = Math.imul(d ^ o.digest, 0x01000193) >>> 0;
  assert.equal(d, DIGEST);
});

// Goal 2's rule gives a silent link 2^BACKOFF - 1 round trips after its probe (a TCP carrier may be
// retransmitting with doubling timers), and a page that was frozen says so (`resumed`) before it
// reads what came while it slept, so its freeze is neither learned as lateness nor judged as silence.
test("bp_g9_s2 a live link is never judged dead (shown and frozen pages)", () => {
  for (const page of ["shown", "frozen"] as const) {
    const dead = run(page, false, N).reduce((n, o) => n + o.falseDead, 0);
    assert.equal(dead, 0, `${page}: ${dead} false dead verdicts in ${N} traces`);
  }
});

test("bp_g9_s2 a page frozen then resumed judges from arrivals and finds a blackhole", () => {
  const lost = run("frozen", true, N).filter((o) => o.foundAt === null).length;
  assert.equal(lost, 0, `${lost} blackholes not found in ${N} traces`);
});
