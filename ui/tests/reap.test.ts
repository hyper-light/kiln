// W24-R31: the reaper's policies as the UI reads and writes them.
import assert from "node:assert/strict";
import { test } from "node:test";
import { bytesText, fieldsOf, fromText, keysOf, policyOf, scopeParam, valueText } from "../src/api/reap.ts";

test("a scope sets only its keys: the hardware limits for the system and a runner alone", () => {
  assert.deepEqual(keysOf("repo"), ["keep_last", "keep_within", "max_age", "schedule"]);
  assert.deepEqual(keysOf("session"), keysOf("repo"));
  assert.ok(keysOf("runner").includes("floor") && keysOf("system").includes("target"));
});

test("a form's fields make a policy: empty fields left to other scopes, keep_last a number", () => {
  assert.deepEqual(policyOf({ floor: "10%", keep_last: "3", max_age: " 14d ", schedule: "" }, "system"), { floor: "10%", keep_last: 3, max_age: "14d" });
  assert.deepEqual(policyOf({ floor: "10%", keep_last: "1" }, "repo"), { keep_last: 1 }, "a repository's hardware fields are left out");
  assert.deepEqual(fieldsOf({ keep_last: 3 }, "session"), { keep_last: "3", keep_within: "", max_age: "", schedule: "" });
});

test("the scope a change names", () => {
  assert.equal(scopeParam("system", ""), "server");
  assert.equal(scopeParam("repo", "acme/app"), "repo:acme/app");
  assert.equal(scopeParam("session", "S1"), "session:S1");
});

test("values in force read as people say them, with where they came from", () => {
  assert.equal(bytesText(200 * 2 ** 30), "200 GiB");
  assert.equal(bytesText(1536), "1.5 KiB");
  assert.equal(valueText("floor", { bytes: 2 ** 40, from: "runner" }), "1 TiB");
  assert.equal(valueText("keep_last", { value: 2, from: null }), "2 per lane");
  assert.equal(valueText("schedule", { value: "30 3 * * *", from: "system" }), "30 3 * * *");
  assert.equal(fromText("repo"), "the repository");
  assert.equal(fromText(null), "kiln's default");
});
