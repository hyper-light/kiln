/** Plan 01a199e1b177752e84dad5dc990e6a15: per-person placement and timed parks. */
import assert from "node:assert/strict";
import { test } from "node:test";
import { attentionRows, isParked, parkChoices } from "../src/views/harness/attention-state.ts";
import { AttentionOrder, AttentionVisits } from "../src/lib/attention-order.ts";
import { requestsFor, requestsForFamily } from "../src/lib/attention-requests.ts";
import { defaultInfo } from "../mock/defaults.ts";
import type { AttentionSnapshot } from "../src/api/attention.ts";
import type { MissionRow } from "../src/views/harness/mission-rows.ts";
const snapshot = (): AttentionSnapshot => ({
  you: "one",
  agents: {},
  order: ["a", "b"],
  archived: [],
  uses: [],
  settings: { sort: "learned", auto_park_days: null, archive_after_days: 7, auto_archive: false },
});
const row: MissionRow = { a: { ...defaultInfo(), id: "a", approvals: [] }, stand: "finished" };
test("mc_attention: parked work wakes on requests, errors and its deadline, not normal activity", () => {
  const one = snapshot();
  one.agents.a = { revision: 1, parked_until: 5000 };
  assert.equal(isParked(row, one, 4000), true);
  assert.equal(isParked({ ...row, stand: "working" }, one, 4000), true);
  assert.equal(isParked({ ...row, stand: "needs" }, one, 4000), false);
  assert.equal(isParked({ ...row, stand: "crashed" }, one, 4000), false);
  assert.equal(isParked(row, one, 5000), false);
  assert.equal(isParked(row, snapshot(), 4000), false, "another person's placement is independent");
});
test("mc_attention: the server rank is retained while live work updates in place", () => {
  const one = snapshot();
  one.agents.a = { revision: 1, pinned: 10, reason: "You opened this chat" };
  const [first] = attentionRows([row], one);
  const [changed] = attentionRows([{ ...row, stand: "working" }], one);
  assert.equal(first?.rank, changed?.rank);
  assert.equal(changed?.stand, "working");
  assert.equal(changed?.pinned, 10);
  assert.equal(changed?.reason, "You opened this chat");
});

test("mc_attention: Park defers captured requests or errors while new and unknown identities wake it", () => {
  const one = snapshot();
  one.agents.a = { revision: 1, parked_at: 2000, parked_until: 5000 };
  const request = { ...row, stand: "needs" as const, a: { ...row.a, held: { why: "Needs a decision", since: 1000 } } };
  one.agents.a.deferred_requests = requestsFor([request.a]).requests;
  assert.equal(isParked(request, one, 4000), true);
  assert.equal(isParked({ ...request, a: { ...request.a, held: { why: "New decision", since: 3000 } } }, one, 4000), false);
  assert.equal(isParked({ ...row, stand: "needs" }, one, 4000), false);
  const stopped = { ...row, stand: "crashed" as const, a: { ...row.a, status: "exited" as const, exit: { code: 1, error: "Script failed", at: 1000 } } };
  one.agents.a.deferred_requests = requestsFor([stopped.a]).requests;
  assert.equal(isParked(stopped, one, 4000), true);
  assert.equal(isParked({ ...stopped, a: { ...stopped.a, exit: { code: 1, error: "New failure", at: 3000 } } }, one, 4000), false);
  one.agents.a.deferred_requests = requestsFor([request.a]).requests;
  assert.equal(isParked({ ...request, a: row.a, asking: request.a }, one, 4000), true);
});

test("mc_attention: a new approval wakes Park even when its runner clock is equal or behind", () => {
  const one = snapshot();
  const approval = { id: "old", tool: "question", tool_name: "question", title: "Pick a branch", at: 8000 };
  const current = { ...row, stand: "needs" as const, a: { ...row.a, approvals: [approval] } };
  one.agents.a = { revision: 1, parked_at: 2000, parked_until: 9000, deferred_requests: requestsFor([current.a]).requests };
  assert.equal(isParked(current, one, 4000), true, "a captured request stays deferred even with an ahead clock");
  for (const at of [8000, 1000]) {
    assert.equal(isParked({ ...current, a: { ...current.a, approvals: [{ ...approval, id: "new", at }] } }, one, 4000), false);
  }
});

test("mc_attention: all true-family request identities matter, and standalone agents stay independent", () => {
  const main = { ...row.a, subagent: false };
  const child = { ...row.a, id: "child", parent: "a", subagent: true, held: { why: "First decision", since: 1000 } };
  const nested = { ...child, id: "nested", parent: "child", held: { why: "Nested decision", since: 1000 } };
  const peer = { ...child, id: "peer", subagent: false };
  const family = [main, child, nested, peer];
  const requests = requestsForFamily(family, "a");
  assert.deepEqual(
    requests.requests.map((r) => r.agent),
    ["child", "nested"],
  );
  const one = snapshot();
  one.agents.a = { revision: 1, parked_until: 9000, deferred_requests: requests.requests };
  assert.equal(isParked({ ...row, requests }, one, 4000), true);
  assert.equal(isParked({ ...row, requests: requestsForFamily([main, child, { ...nested, held: { why: "New nested decision", since: 1 } }, peer], "a") }, one, 4000), false);
});

test("mc_attention: missing request identity or time wakes conservatively", () => {
  const one = snapshot();
  one.agents.a = { revision: 1, parked_until: 9000, deferred_requests: [] };
  const unknown = [
    { ...row.a, status: "waiting" as const },
    { ...row.a, held: { why: "", since: 1000 } },
    { ...row.a, held: { why: "Known request", since: 0 } },
    { ...row.a, status: "exited" as const, exit: { code: 1, error: "", at: 1000 } },
  ];
  for (const a of unknown) assert.equal(isParked({ ...row, a, requests: requestsFor([a]) }, one, 4000), false);
});
test("mc_attention: park presets are future local-day settings", () => {
  const now = new Date(2026, 10, 1, 20).getTime();
  const choices = parkChoices(now);
  assert.ok(choices.every((c) => c.at > now));
  assert.ok(!choices.some((c) => c.value === "evening"));
  const tomorrow = new Date(choices.find((c) => c.value === "tomorrow")?.at ?? 0);
  assert.equal(tomorrow.getDate(), 2);
  assert.equal(tomorrow.getHours(), 9);
});

test("mc_attention: reconnects cannot reorder a visit; return and own action can", () => {
  const order = new AttentionOrder();
  assert.deepEqual(order.take(["a", "b"]), ["a", "b"]);
  assert.deepEqual(order.take(["b", "a"]), ["a", "b"], "routine reload or reconnect");
  assert.deepEqual(order.take(["b", "a"], true), ["b", "a"], "a person's action");
  order.returned();
  assert.deepEqual(order.take(["a", "b"]), ["a", "b"], "new visit");
  assert.deepEqual(new AttentionOrder().take(["b", "a"]), ["b", "a"], "another person's order");
});

test("mc_attention: frozen orders retain their actual exposure; own actions and arrivals replace both", () => {
  const order = new AttentionOrder();
  const first = { order: ["a", "b"], exposure: "shown", status: "initial" };
  assert.deepEqual(order.snapshot(first), first);
  assert.deepEqual(order.snapshot({ order: ["b", "a"], exposure: "unshown", status: "updated" }), { ...first, status: "updated" });
  assert.equal(order.snapshot({ order: ["b", "a"], exposure: "own" }, true).exposure, "own");
  order.returned();
  assert.equal(order.snapshot({ order: ["a", "b"], exposure: "arrival" }).exposure, "arrival");
});

test("mc_attention: return gets a fresh resource key while repeated renders and resubscriptions retain it", () => {
  const visits = new AttentionVisits();
  const first = visits.arrive();
  visits.subscribed();
  assert.equal(visits.arrive(), first);
  visits.returned();
  visits.subscribed();
  assert.equal(visits.arrive(), first, "React resubscription is not a new visit");
  visits.returned();
  assert.notEqual(visits.arrive(), first, "navigation back cannot reuse a cached resource");
});

test("mc_attention: urgency uses server order and no global history never marks a request stale", () => {
  const one = snapshot();
  one.request_order = ["b", "a"];
  one.agents.a = { revision: 1, urgency: { known: false, score: null, stale: false, since: 1, waited: 1, expected_wait: null, observations: 0, reason: "No answer history" } };
  one.agents.b = { revision: 1, urgency: { known: true, score: 4, stale: true, since: 1, waited: 4, expected_wait: 1, observations: 1, reason: "Waiting longer than previous answered requests" } };
  const [unknown, stale] = attentionRows(
    [
      { ...row, stand: "needs" },
      { ...row, a: { ...row.a, id: "b" }, stand: "needs" },
    ],
    one,
  );
  assert.equal(unknown?.stale, false);
  assert.equal(unknown?.requestRank, 1);
  assert.equal(stale?.stale, true);
  assert.equal(stale?.requestRank, 0);
});

test("mc_attention: a new request kind can still be stale against the global answered history", () => {
  const one = snapshot();
  one.agents.a = {
    revision: 1,
    urgency: { known: false, score: null, stale: true, since: 1, waited: 10, expected_wait: null, observations: 0, reason: "Waiting longer than all previously answered requests" },
  };
  assert.equal(attentionRows([{ ...row, stand: "needs" }], one)[0]?.stale, true);
});

test("mc_attention: the person's park preset settings control duration and local calendar times", () => {
  const now = new Date(2026, 10, 1, 12).getTime();
  const choices = parkChoices(now, { park_later_minutes: 90, park_evening_hour: 20, park_morning_hour: 7 });
  assert.equal(choices.find((c) => c.value === "later")?.at, now + 90 * 60_000);
  assert.equal(new Date(choices.find((c) => c.value === "evening")?.at ?? 0).getHours(), 20);
  const morning = new Date(choices.find((c) => c.value === "tomorrow")?.at ?? 0);
  assert.equal(morning.getDate(), 2);
  assert.equal(morning.getHours(), 7);
});
