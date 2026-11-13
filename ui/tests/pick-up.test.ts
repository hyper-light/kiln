// A past conversation picked up again from the composer (kiln plan 01a1d2c16eb074f19cf7ae7f1025c0be step 5): one that ran in a
// microVM goes on in one, made from the Agentfile kept for its harness in its folder, else kiln's for its harness; one of the
// machine's own goes on as a process, as before.
import { test } from "node:test";
import assert from "node:assert/strict";
import type { PastSession } from "../src/api/harness.ts";
import type { AgentfileListing } from "../src/api/wire/index.ts";
import { resumedImage, withChoice } from "../src/views/agentfiles/choose.ts";
import { resumeSpecOf } from "../src/views/harness/spawn-setup.ts";

const listing: Pick<AgentfileListing, "agentfiles" | "builtin"> = {
  agentfiles: [{ id: "01a1cccccccccccccccccccccccccccc", owner: "owner", name: "codex-rust", version: 2, created_at: 1, updated_at: 2 }],
  builtin: [
    { name: "harness-codex", harness: "codex", text: "FROM debian\n" },
    { name: "harness-claude", harness: "claude", text: "FROM debian\n" },
  ],
};

const past = (over: Partial<PastSession> = {}): PastSession => ({ kind: "codex", id: "thread-1", title: "Port the parser to Rust", cwd: "/home/ada/src/kiln", last_activity: 1, ...over });
const inVolume = past({ isolation: { kind: "microvm", person: "owner" } });

test("step35: a conversation that ran in a microVM goes on in one, from kiln's Agentfile for its harness where none is kept for its folder", () => {
  const image = resumedImage(listing, {}, inVolume.kind, inVolume.cwd);
  assert.equal(image, "harness-codex");
  assert.deepEqual(resumeSpecOf(inVolume, { mode: "auto", effort: "high", image }), {
    kind: "codex",
    cwd: "/home/ada/src/kiln",
    resume: "thread-1",
    mode: "auto",
    effort: "high",
    name: "Port the parser to Rust",
    isolation: { kind: "microvm", image: "harness-codex" },
  });
});

test("step35: the Agentfile kept for its harness in its folder, as typed or under ~, is the one it goes on in", () => {
  const mine = "agentfile:01a1cccccccccccccccccccccccccccc";
  assert.equal(resumedImage(listing, withChoice({}, "codex", "/home/ada/src/kiln", mine), "codex", "/home/ada/src/kiln"), mine);
  assert.equal(resumedImage(listing, withChoice({}, "codex", "~/src/kiln", mine), "codex", "/home/ada/src/kiln"), mine);
  // Kept for another folder, or for another harness there: kiln's for its harness.
  assert.equal(resumedImage(listing, withChoice({}, "codex", "~/src/shards", mine), "codex", "/home/ada/src/kiln"), "harness-codex");
  assert.equal(resumedImage(listing, withChoice({}, "claude", "~/src/kiln", mine), "codex", "/home/ada/src/kiln"), "harness-codex");
  // None kept there (a process, for a new agent): still a microVM for one that ran in one.
  assert.equal(resumedImage(listing, withChoice({}, "codex", "~/src/kiln", null), "codex", "/home/ada/src/kiln"), "harness-codex");
});

test("step35: with no Agentfile known, it still goes on in a microVM, the server naming kiln's for its harness", () => {
  const image = resumedImage({ agentfiles: [], builtin: [] }, {}, "codex", inVolume.cwd);
  assert.equal(image, undefined);
  assert.deepEqual(resumeSpecOf(inVolume, { mode: "", image }).isolation, { kind: "microvm" });
});

test("step35: one of the machine's own goes on as a process, as before", () => {
  const spec = resumeSpecOf(past(), { mode: "", effort: undefined, image: "harness-codex" });
  assert.equal("isolation" in spec, false);
  assert.deepEqual(spec, { kind: "codex", cwd: "/home/ada/src/kiln", resume: "thread-1", mode: undefined, effort: undefined, name: "Port the parser to Rust" });
});
