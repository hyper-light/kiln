/**
 * The Agents page's experiments stay live as their sessions come and go (kiln plan 01a1dd2ea7a777a28970a43fc5f766d0
 * P7.2): the list is read again where a session an experiment drew changes what it shows (its variant's count as it
 * starts, its measures as its first start is measured and as it ends), and at nothing else.
 */
import assert from "node:assert/strict";
import { test } from "node:test";
import { drawChanged, type DrawnFacts } from "../src/api/agentdefs";
import type { ServerEvent } from "../src/api/types";

/** Session `id`'s harness event: its status, whether its first start was measured, and the experiment that drew it, where one did. */
const harness = (id: string, status: string, experiment?: string, measured = false) =>
  ({
    type: "harness",
    info: { id, status, starts: measured ? [{ at: 1, runs: {}, measured: { start_us: 1 } }] : [], ...(experiment ? { experiment: { experiment, arm: "v1", promoted: false } } : {}) },
  }) as unknown as ServerEvent;
const closed = (id: string): ServerEvent => ({ type: "harness_closed", id });

test("p7_2 a drawn session's experiment is read again as it starts, measures its first start and ends, and at nothing else", () => {
  const seen = new Map<string, DrawnFacts>();
  assert.equal(drawChanged(harness("s1", "starting", "e1"), seen), true, "drawn: its variant's count");
  assert.equal(drawChanged(harness("s1", "running", "e1"), seen), false, "at work: nothing it measures changed");
  assert.equal(drawChanged(harness("s1", "running", "e1", true), seen), true, "its first start measured");
  assert.equal(drawChanged(harness("s1", "idle", "e1", true), seen), false);
  assert.equal(drawChanged(harness("s1", "exited", "e1", true), seen), true, "over: its end and its peak");
  assert.equal(drawChanged(closed("s1"), seen), false, "closed once it said it was over");
  assert.equal(seen.size, 0, "let go as it closes");
});

test("p7_2 a drawn session closing without saying it was over is read again, and a session no experiment drew never is", () => {
  const seen = new Map<string, DrawnFacts>();
  assert.equal(drawChanged(harness("s2", "running", "e1"), seen), true);
  assert.equal(drawChanged(closed("s2"), seen), true, "the server reads it over as it goes");
  assert.equal(seen.size, 0);
  for (const ev of [harness("s3", "running"), harness("s3", "exited"), closed("s3")]) assert.equal(drawChanged(ev, seen), false, ev.type);
  assert.equal(seen.size, 0, "nothing kept of it");
});
