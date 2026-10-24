import assert from "node:assert/strict";
import { test } from "node:test";
import { lostLine } from "../src/views/runner-lost.ts";

test("a runner page says what was lost below the channel, each side's, the most first", () => {
  assert.equal(lostLine({}), "none");
  assert.equal(lostLine({ dropped: [{ lane: "term", reason: "chaos", frames: 0 }] }), "none");
  assert.equal(
    lostLine({
      dropped: [
        { lane: "logs", reason: "shed", frames: 2 },
        { lane: "term", reason: "chaos", frames: 9 },
      ],
      dropped_toward: [{ lane: "control", reason: "stuck", frames: 1 }],
    }),
    "11 here (term chaos 9, logs shed 2) · 1 at the server (control stuck 1)",
  );
});
