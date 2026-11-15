import assert from "node:assert/strict";
import { test } from "node:test";
import { kindsByTarget, type KindInfo } from "../src/api/harness.ts";

const kind = (id: string, available = true) => ({ id, name: id, verified: true, available, modes: [], default_mode: "", structured: true, program: "" }) as unknown as KindInfo;

test("W21-R02 the kinds each target offers, read from lists written once: each target has its own, and those that offer alike hold the one list", () => {
  const both = [kind("claude"), kind("codex")];
  const one = [kind("claude"), kind("codex", false)];
  const by = kindsByTarget({ lists: [both, one, []], targets: { local: 0, "agent:a": 0, "agent:b": 1, "agent:c": 0, "agent:d": 2 } });
  assert.deepEqual(Object.keys(by).sort(), ["agent:a", "agent:b", "agent:c", "agent:d", "local"]);
  assert.deepEqual(by["agent:b"], one);
  assert.deepEqual(by["agent:d"], []);
  assert.equal(by.local, by["agent:a"], "one list, held by both");
  assert.equal(by["agent:c"], both);
  // A list that isn't there is none, not a crash.
  assert.deepEqual(kindsByTarget({ lists: [both], targets: { local: 4 } }), { local: [] });
});

test("W21-R02 a server from before the lists were written once answers by target, and is read as it answers", () => {
  const by = { local: [kind("claude")], "agent:a": [kind("codex")] };
  assert.equal(kindsByTarget(by), by);
  assert.deepEqual(kindsByTarget({}), {});
});
