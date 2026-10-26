import assert from "node:assert/strict";
import { test } from "node:test";
import { unsending } from "../src/views/harness/unsend";

test("CP-T3.2: unsending a queued message asks to take it back under its own id; undoing that sends nothing again", () => {
  const { unsend, undo } = unsending("m2");
  assert.deepEqual(unsend, { type: "queue_remove", id: "m2" });
  // Its harness (or adapter) holds it, so it still goes, once: sent again under a new id, it was a
  // second prompt (the twentieth adversarial check's).
  assert.deepEqual(undo, [], "undo sends nothing");
  assert.equal(unsending(undefined).unsend, undefined, "with no id, nothing to take back");
});
