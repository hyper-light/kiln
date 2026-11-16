/**
 * W21-R02: a lean attention snapshot reads back as the snapshot. The fixture is what the server writes of one
 * (`crates/kiln/src/harness/attention/lean.rs` holds it to that), and what the server's own reader makes of it.
 */
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fromLean, isLean, type LeanSnapshot } from "../src/api/attention-lean.ts";
import type { AttentionSnapshot } from "../src/api/attention.ts";

const fixture = JSON.parse(readFileSync(new URL("./fixtures/attention-lean.json", import.meta.url), "utf8")) as { lean: LeanSnapshot; snapshot: AttentionSnapshot; said: Record<string, string> };

test("W21-R02 a lean snapshot read back is the snapshot: every mark its agent's, every order its agents in their places", () => {
  assert.ok(isLean(fixture.lean));
  assert.equal(isLean(fixture.snapshot), false, "the snapshot itself isn't a lean one");
  const { snapshot, said } = fromLean(fixture.lean);
  assert.deepStrictEqual(snapshot, fixture.snapshot);
  assert.deepStrictEqual(said, fixture.said);
  // The fixture's lists are said each way: the order and the visit's second order as a number, its first by places.
  assert.equal(typeof fixture.lean.order, "number");
  assert.equal(fixture.lean.pair?.order_b, fixture.lean.order);
  assert.ok(Array.isArray(fixture.lean.pair?.order_a));
  assert.equal(snapshot.order.length, 6);
  assert.equal(snapshot.pair?.order_a.length, 7, "an agent only the visit's first order names is named too");
  assert.equal(Object.keys(snapshot.agents).length, 8, "the marks of agents the order doesn't have are theirs too");
  // What a lean one leaves out isn't made up: no score on a mark, no report.
  assert.equal("model" in snapshot, false);
  assert.ok(Object.values(snapshot.agents).every((mark) => !("rank" in mark)));
});

test("W21-R02 a lean snapshot carries the last words of the rows it was asked for, by agent, and none unasked", () => {
  assert.equal(Object.keys(fixture.said).length, 3, "the list's first three rows, one of which has said nothing, and its last");
  const unasked = { ...fixture.lean };
  delete unasked.said;
  assert.deepStrictEqual(fromLean(unasked).said, {});
});
