import assert from "node:assert/strict";
import { test } from "node:test";
import type { AgentChange } from "../src/api/changes.ts";
import { changeState } from "../src/views/harness/change-state.ts";

const open: AgentChange = {
  agent: "a1",
  provider: "kiln",
  change: "CR12",
  url: "",
  branch: "agents/a1",
  trunk: "main",
  sha: "abc",
  version: 2,
  files: [],
  title: "t",
  state: "open",
  fix_checks: true,
  opened_at: 1,
  updated_at: 2,
};

const word = (c: Partial<AgentChange> | undefined) => changeState(c ? { ...open, ...c } : undefined).word;

test("a change request's state is the one thing it waits on first", () => {
  assert.equal(word(undefined), "Pending");
  assert.equal(word({}), "In review");
  assert.equal(word({ review: "approved", checks: "passed" }), "Approved");
  assert.equal(word({ checks: "running" }), "Checking");
  assert.equal(word({ checks: "queued", review: "approved" }), "Checking", "not approved while checks still run");
  assert.equal(word({ checks: "failed", fix_checks: false, review: "approved" }), "Failed", "failing checks first");
  assert.equal(word({ checks: "failed" }), "Fixing", "the agent fixes them by itself");
  assert.equal(word({ review: "changes-requested", checks: "running" }), "Changes requested");
  assert.equal(word({ change: undefined }), "Pushed", "its branch pushed, its pull request not opened yet");
  assert.equal(word({ checks: "cancelled" }), "In review", "checks that ended without failing hold nothing up");
  assert.equal(word({ state: "landed", checks: "failed" }), "Merged");
  assert.equal(word({ state: "closed" }), "Closed");
});

test("each state shows as one of kiln's status dots", () => {
  const dots = new Set(["pending", "running", "failed", "timed_out", "succeeded", "cancelled"]);
  for (const c of [undefined, {}, { checks: "failed" }, { checks: "failed", fix_checks: false }, { review: "changes-requested" }, { state: "landed" }, { state: "closed" }]) {
    assert.ok(dots.has(changeState(c ? { ...open, ...c } : undefined).dot));
  }
});
