// The catalogue's names and its normal form (docs/browser-lab.md, "The operations"; plan
// 01a1a832e9cc step 3): ids out of operations' names, and what differs between two runs of one
// seed out of a frame before it is hashed.
import { test } from "node:test";
import assert from "node:assert/strict";
import { normal, pattern } from "../catalogue.mjs";

test("g1_s3 an operation's name has no ids and keeps its query's keys", () => {
  assert.equal(pattern("https://10.77.0.1:8443/api/harness/sessions/01a1a94fe73a77a1a8fff1e1d19098c1/ws?z=gzip&patch=state&patch=changes"), "/api/harness/sessions/:id/ws?patch&z");
  assert.equal(pattern("https://10.77.0.1:8443/api/agents"), "/api/agents");
  assert.equal(pattern("https://10.77.0.1:8443/api/runs/12/jobs"), "/api/runs/:n/jobs");
});

test("g1_s3 two runs' frames on one seed read the same once stamps and ids are out", () => {
  const a = '{"id":"msg_01a1a8f21355_201_0:0","seq":8362,"at":1793836525502,"text":"@lab:1793836525502235 why the tick"}';
  const b = '{"id":"msg_01a1a90f981b_201_0:0","seq":12,"at":1793839999999,"text":"@lab:1793839999999123 why the tick"}';
  assert.equal(normal(a), normal(b));
  assert.notEqual(normal(a), normal(a.replace("why", "who")));
});
