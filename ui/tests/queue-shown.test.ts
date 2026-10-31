import assert from "node:assert/strict";
import { test } from "node:test";
import { notShown } from "../src/views/harness/queue-shown";

test("CP-T3.2: a queued message the conversation shows where it was said isn't listed again above the composer", () => {
  const queue = [{ id: "held" }, { id: "m2" }];
  // Held for Codex as it started, then the agent switched: the conversation shows it queued.
  assert.deepEqual(notShown(queue, { held: "queued" }), [{ id: "m2" }]);
  // Nothing of it shown: the same list, as it was.
  assert.equal(notShown(queue, { other: "completed" }), queue);
  assert.equal(notShown(queue, undefined), queue);
  assert.deepEqual(notShown([], { held: "queued" }), []);
});
