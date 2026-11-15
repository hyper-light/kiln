// The attach form saves only what the person set (kiln plan 01a1dd2e8618733a87f2793aab1322a8 P5.1b): an
// Agent left as it is stays unset, so the person's own copy of kiln's agent wins on the server
// (`ci::attach::tests::p5_1b_…`, which saves this same shape); a harness they pick is kept as theirs.
import { test } from "node:test";
import assert from "node:assert/strict";
import { defaultsFor, KILNS } from "../src/views/attach/defaults.ts";

test("p5_1b: an Agent left as it is saves no harness kind; one picked saves it", () => {
  const left = defaultsFor({ name: "ci-doctor", on: "repo:acme/app", harness: KILNS, wake: ["failure"], playbook: "", machine: "local", cwd: "/w" });
  assert.deepEqual(left.harness, { on: "local", cwd: "/w" });
  assert.equal("kind" in left.harness, false);
  const picked = defaultsFor({ name: "ci-doctor", on: "repo:acme/app", harness: "codex", wake: ["failure"], playbook: "" });
  assert.deepEqual(picked.harness, { kind: "codex" });
});
