import assert from "node:assert/strict";
import { test } from "node:test";
import { waitSentence } from "../src/api/ci.ts";

test("an adopter's wait names the execution it adopted and that execution's run (W28-R47)", () => {
  assert.equal(waitSentence({ reason: "adopted", leg: "A/rust", run: "A" }), "Adopted A/rust of run A: the same work in flight, which it finishes reused from.");
  assert.equal(waitSentence({ reason: "adopted" }), "Adopted an execution: the same work in flight, which it finishes reused from.");
});
