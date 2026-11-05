import assert from "node:assert/strict";
import { test } from "node:test";
import { offlineLabel, staleWhy } from "../src/views/runner-stale.ts";

// docs/machines.md, "Records that are stale": a record that never said its machine id is shown as stale, with why; kiln leaves forgetting it to a person.
test("machines: a record that never said its machine is shown as stale, saying why", () => {
  assert.equal(offlineLabel({ stale: true }), "stale");
  assert.equal(offlineLabel({}), "offline");
  assert.equal(offlineLabel({ stale: false }), "offline");
  assert.match(staleWhy({ stale: true, connected: false }) ?? "", /never said its machine id.*Forget it/);
  assert.equal(staleWhy({ connected: false }), null);
  assert.equal(staleWhy({ stale: true, connected: true }), null, "a connected one isn't stale");
});
