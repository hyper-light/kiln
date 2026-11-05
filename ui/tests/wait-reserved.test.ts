import assert from "node:assert/strict";
import { test } from "node:test";
import { waitSentence } from "../src/api/ci.ts";

const at = Date.UTC(2026, 9, 30, 14, 5);
const clock = new Date(at).toLocaleTimeString();

test("a leg with room kept for it says where and from when (W05-R39)", () => {
  assert.equal(
    waitSentence({ reason: "reserved", runner: "fedora-2", at }),
    `No runner has room for it now: room is kept for it on fedora-2 from ${clock}, when the work there is expected to have left it room.`,
  );
  assert.equal(waitSentence({ reason: "reserved" }), "No runner has room for it now: room is kept for it on a runner from when it frees, when the work there is expected to have left it room.");
});

test("a leg kept out of another's room says whose, where and until when (W05-R39)", () => {
  assert.equal(
    waitSentence({ reason: "behind", leg: "r12/gate", runner: "fedora-2", at }),
    `It would fit on fedora-2, but only in room kept there for r12/gate from ${clock}, past which it would still run.`,
  );
});

test("a gang's member waiting for the rest of its gang says how many are queued (W05-R40)", () => {
  assert.equal(waitSentence({ reason: "gang", gang: "sim", have: 2, of: 3 }), "Waiting for the rest of its gang sim, which starts together: 2 of 3 queued.");
});
