/** Plan 01a199e1b177752e84dad5dc990e6a15: per-person placement and timed parks. */
import assert from "node:assert/strict";
import { test } from "node:test";
import { attentionRows, comparisonLabel, isParked, organizedLocally, parameterLabel, parkChoices, untilWords } from "../src/views/harness/attention-state.ts";
import { AttentionOrder, AttentionVisits } from "../src/lib/attention-order.ts";
import { requestsFor, requestsForFamily } from "../src/lib/attention-requests.ts";
import { defaultInfo } from "../mock/defaults.ts";
import type { AttentionSettings, AttentionSnapshot } from "../src/api/attention.ts";
import type { MissionRow } from "../src/views/harness/mission-rows.ts";
const settings: AttentionSettings = { sort: "learned", auto_park_days: null, archive_after_days: 7, auto_archive: true, park_later_minutes: 60, park_evening_hour: 18, park_morning_hour: 9 };
const snapshot = (): AttentionSnapshot => ({
  you: "one",
  agents: {},
  order: ["a", "b"],
  request_order: [],
  archived: [],
  settings,
  exposure: null,
  pair: null,
  model: null,
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

test("mc_attention: a row says what holds it where it is, and a correction of an automatic park says so", () => {
  const one = snapshot();
  one.agents.a = { revision: 2, rank_reason: "Your latest interaction" };
  assert.equal(attentionRows([row], one)[0]?.reason, "Your latest interaction", "where the learned order put it");
  one.agents.a = { revision: 3, rank_reason: "Your latest interaction", reason: "You brought it back", correction: true };
  const [back] = attentionRows([row], one);
  assert.equal(back?.reason, "You brought it back", "why it came back stands over its rank");
  assert.equal(back?.correction, true);
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
  const choices = parkChoices(now, settings);
  assert.ok(choices.every((c) => c.at > now));
  assert.ok(!choices.some((c) => c.value === "evening"));
  const tomorrow = new Date(choices.find((c) => c.value === "tomorrow")?.at ?? 0);
  assert.equal(tomorrow.getDate(), 2);
  assert.equal(tomorrow.getHours(), 9);
});

test("mc_attention: reconnects cannot reorder a visit; a return draws again", () => {
  const order = new AttentionOrder();
  assert.deepEqual(order.take(["a", "b"]), ["a", "b"]);
  assert.deepEqual(order.take(["b", "a"]), ["a", "b"], "routine reload or reconnect");
  order.returned();
  assert.deepEqual(order.take(["b", "a"]), ["b", "a"], "new visit");
  assert.deepEqual(new AttentionOrder().take(["b", "a"]), ["b", "a"], "another person's order");
});

test("mc_attention: your own action moves only the rows it acted on, to their new rank; every other row stays", () => {
  const order = new AttentionOrder();
  order.take(["a", "b", "c", "d", "e"]);
  // The server rescored c (acted on) and, by its own draw, would show the rest otherwise too.
  order.place(["c"], ["c", "e", "d", "b", "a"]);
  assert.deepEqual(order.order, ["c", "a", "b", "d", "e"], "c goes where its new rank puts it; a, b, d and e keep their places");
  order.place(["b"], ["a", "c", "d", "e", "b"]);
  assert.deepEqual(order.order, ["c", "a", "d", "e", "b"]);
  order.place(["d"], ["a", "c", "e", "b"]);
  assert.deepEqual(order.order, ["c", "a", "e", "b"], "one the server no longer ranks (archived) leaves the order");
  order.place(["d"], ["c", "a", "d", "e", "b"]);
  assert.deepEqual(order.order, ["c", "a", "d", "e", "b"], "and comes back where it ranks (restored)");
  const before = [...order.order];
  order.place(["a", "e"], ["c", "d", "b"]);
  order.place(["a", "e"], before);
  assert.deepEqual(order.order, before, "Undo puts each back where it was");
});

test("mc_attention: frozen orders retain their actual exposure; arrivals replace it", () => {
  const order = new AttentionOrder();
  const first = { order: ["a", "b"], exposure: "shown", status: "initial" };
  assert.deepEqual(order.snapshot(first), first);
  assert.deepEqual(order.snapshot({ order: ["b", "a"], exposure: "unshown", status: "updated" }), { ...first, status: "updated" });
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

test("mc_attention: what an action shows at once: pins, parks with the requests it defers, archives and settings", () => {
  const one = snapshot();
  const waiting = { ...row.a, held: { why: "Needs a decision", since: 1000 } };
  const parked = organizedLocally(one, [{ action: "park", agent: "a", until: 9000 }], 2000, [waiting]);
  assert.equal(parked.agents.a?.parked_until, 9000);
  assert.equal(parked.agents.a?.reason, "You parked it");
  assert.deepEqual(parked.agents.a?.deferred_requests, requestsFor([waiting]).requests, "a known request waits with it");
  assert.equal(isParked({ ...row, stand: "needs", a: waiting }, parked, 3000), true);
  const pinned = organizedLocally(parked, [{ action: "pin", agent: "a" }], 3000, [waiting]);
  assert.equal(pinned.agents.a?.pinned, 3000);
  assert.equal(pinned.agents.a?.parked_until, undefined, "a pin ends its park");
  const automatic = organizedLocally(
    { ...one, agents: { a: { revision: 4, parked_at: 1000, parked_until: 9000, automatic: true, reason: "Automatically parked: no interaction or turns for 3 days" } } },
    [{ action: "unpark", agent: "a" }],
    3500,
    [],
  );
  assert.equal(automatic.agents.a?.correction, true, "bringing back kiln's own park is a correction");
  assert.equal(automatic.agents.a?.automatic, undefined);
  assert.equal(automatic.agents.a?.parked_until, undefined);
  assert.equal(automatic.agents.a?.woke_at, 3500);
  assert.equal(organizedLocally({ ...one, agents: { a: { revision: 1, parked_until: 9000 } } }, [{ action: "unpark", agent: "a" }], 3500, []).agents.a?.correction, undefined, "your own park isn't");
  const archived = organizedLocally(one, [{ action: "archive", agent: "a" }], 3000, [row.a]);
  assert.deepEqual(
    archived.archived.map((a) => a.id),
    ["a"],
  );
  assert.equal(organizedLocally(archived, [{ action: "restore", agent: "a" }], 4000, []).archived.length, 0);
  assert.equal(organizedLocally(one, [{ action: "seen", agent: "a" }], 5000, []).agents.a?.updated_seen, 5000);
  assert.equal(organizedLocally(one, [{ action: "settings", settings: { sort: "yours" } }], 5000, []).settings.sort, "yours");
  assert.equal(one.agents.a, undefined, "the snapshot itself is untouched");
});

test("mc_attention: the learned order's names are words, and unknown ones fall back to words", () => {
  assert.equal(comparisonLabel("act_r"), "Learned order");
  assert.equal(comparisonLabel("act_r_with_range"), "Learned order with time span");
  assert.equal(comparisonLabel("your_recency"), "Your most recent use");
  assert.equal(comparisonLabel("without_agent_turns"), "Learned from your uses only");
  assert.equal(comparisonLabel("latest_activity"), "Latest activity (today's order)");
  assert.equal(comparisonLabel("human_recency"), "Human recency");
  assert.equal(parameterLabel("decay"), "Decay");
  assert.equal(parameterLabel("weight.message"), "Weight of your messages");
  assert.equal(parameterLabel("weight.subagent_turn"), "Weight of its subagents' turns");
  assert.equal(parameterLabel("range_weight"), "Weight of time span");
  assert.equal(parameterLabel("scale.turn"), "Strength of its turns, relative to a pick");
  assert.equal(parameterLabel("weight.new_kind"), "Weight of new kind");
  for (const label of [comparisonLabel("some_new.order"), parameterLabel("scale.odd_kind")]) assert.doesNotMatch(label, /[._]/);
});

test("mc_attention: when a park ends, in words", () => {
  const now = new Date(2026, 10, 1, 12).getTime();
  assert.match(untilWords(new Date(2026, 10, 1, 18).getTime(), now), /^today at /);
  assert.match(untilWords(new Date(2026, 10, 2, 9).getTime(), now), /^tomorrow at /);
  assert.doesNotMatch(untilWords(new Date(2026, 10, 9, 9).getTime(), now), /^(today|tomorrow)/);
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
