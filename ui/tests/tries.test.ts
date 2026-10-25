import assert from "node:assert/strict";
import { test } from "node:test";
import { Tries } from "../src/lib/tries";

test("a request tried again keeps its key; another request, or one after it went, gets a new one", () => {
  let n = 0;
  const tries = new Tries(() => `k${++n}`);
  const first = tries.keyFor(JSON.stringify({ plan: "p1", mode: "goals" }));
  assert.equal(tries.keyFor(JSON.stringify({ plan: "p1", mode: "goals" })), first, "the same request, tried again");
  const other = tries.keyFor(JSON.stringify({ plan: "p1", mode: "agents" }));
  assert.notEqual(other, first, "another request");
  tries.done();
  assert.notEqual(tries.keyFor(JSON.stringify({ plan: "p1", mode: "agents" })), other, "after it went, the same words are a new request");
});
