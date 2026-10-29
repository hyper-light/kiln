import assert from "node:assert/strict";
import { test } from "node:test";
import { pinnedSentence, placementWords, retriedWords, retryBody, retryChoices, type RetryOptions } from "../src/api/retry.ts";

const runner = (name: string, more: Partial<RetryOptions["runners"][number]> = {}) => ({ name, connected: true, draining: false, busy: 0, slots: 2, able: true, why: [], ...more });

const options = (more: Partial<RetryOptions> = {}): RetryOptions => ({
  job: "j1",
  run: "r1",
  key: "build",
  kind: "pipeline",
  ran_on: "mac-a",
  elsewhere: true,
  retry: { can: true, why: null },
  runners: [
    runner("mac-a"),
    runner("mac-b", { busy: 2 }),
    runner("mac-c", { connected: false }),
    runner("mac-d", { draining: true }),
    runner("linux", { able: false, why: ["lacks the label macos"] }),
  ],
  ...more,
});

test("a finished job runs again anywhere, anywhere but where it ran, or on a runner able to, each saying how it stands", () => {
  assert.deepEqual(retryChoices(options()), [
    { value: "", label: "Any runner that can run it" },
    { value: "elsewhere", label: "Any runner but mac-a" },
    { value: "runner:mac-a", label: "On mac-a (ran here)" },
    { value: "runner:mac-b", label: "On mac-b (busy)" },
    { value: "runner:mac-c", label: "On mac-c (offline)" },
    { value: "runner:mac-d", label: "On mac-d (draining)" },
  ]);
  // A pod made for it can't be avoided, and a runner unable to run it isn't offered.
  assert.deepEqual(retryChoices(options({ ran_on: "kiln-pod-0", elsewhere: false, runners: [runner("linux", { able: false, why: ["lacks the label k8s"] })] })), [
    { value: "", label: "Any runner that can run it" },
  ]);
});

test("each choice is the retry's request; a runner named `elsewhere` is still a runner", () => {
  assert.deepEqual(retryBody(""), {});
  assert.deepEqual(retryBody("elsewhere"), { elsewhere: true });
  assert.deepEqual(retryBody("runner:mac-b"), { runner: "mac-b" });
  assert.deepEqual(retryBody("runner:elsewhere"), { runner: "elsewhere" });
});

test("a retry says where it runs, and a leg waiting for its runner says why", () => {
  assert.equal(placementWords({ runner: "mac-b" }), "on mac-b");
  assert.equal(placementWords({ avoid: "mac-a" }), "anywhere but mac-a");
  assert.equal(placementWords(null), null);
  assert.equal(retriedWords({ kind: "pipeline", run: "r1", attempt: 1, placement: { avoid: "mac-a" } }), "Retrying: it runs anywhere but mac-a");
  assert.equal(retriedWords({ kind: "ci", run: "r1", attempt: 3, key: "build", placement: { runner: "mac-b" } }), "Attempt 3 of the run started: its leg runs on mac-b");
  assert.equal(retriedWords({ kind: "github-actions", run: "r1" }), "Retrying");
  assert.equal(pinnedSentence("mac-b", "busy"), "Waiting for room on mac-b, where its retry asked it to run.");
  assert.equal(pinnedSentence("mac-b", "offline"), "Waiting for mac-b, where its retry asked it to run, to connect.");
  assert.equal(pinnedSentence("mac-b", "unable"), "mac-b, where its retry asked it to run, can't run it now: retry it elsewhere.");
});
