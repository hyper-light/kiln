/** The Programs view's words (views/programs/words.ts; CP-M3, docs/programs.md). */
import { test } from "node:test";
import assert from "node:assert/strict";
import type { Program, ProgramAgent, ProgramTrack } from "../src/api/programs.ts";
import { changeWords, edited, goalWords, noticeWords, passWords, pauseWords, slotWords, thresholdWords, usageWords } from "../src/views/programs/words.ts";

const track = (id: string, weight: number, more: Partial<ProgramTrack> = {}): ProgramTrack => ({ id, name: id, plan: `p-${id}`, weight, worker: null, goal: null, met: 0, total: 1, pass: 0, slot: "waiting", changes: [], ...more });
const agent = (more: Partial<ProgramAgent> = {}): ProgramAgent => ({ id: "01a15c10306176b4", name: "Hex T7", status: "idle", ...more });
const NOW = 1_800_000_000_000;

test("a slot says where the track stands, a track paused or held for usage says so", () => {
  assert.equal(slotWords("running").text, "at work");
  assert.deepEqual(slotWords("paused"), { text: "paused for usage", tone: "fail" });
  assert.deepEqual(slotWords("held"), { text: "held for usage", tone: "warn" });
  assert.equal(slotWords("done").tone, "dim");
});

test("the usage rule is said with its threshold, and what it does", () => {
  const at = (pct: number, rule: Program["rule"]) => usageWords({ usage: { pct }, rule });
  assert.equal(usageWords({ usage: null, rule: { rule: "ok" } }), null);
  assert.match(at(42, { rule: "ok" })?.text ?? "", /^42% of a weekly window: under 80%/);
  assert.deepEqual(at(87.4, { rule: "hold", why: "" }), { text: "87% of a weekly window: above 80%, no new goal starts (the goals under way go on)", tone: "warn" });
  assert.equal(at(96, { rule: "pause", why: "" })?.tone, "fail");
  assert.match(at(96, { rule: "pause", why: "" })?.text ?? "", /above 95%, the program's goal stacks pause and no new goal starts/);
});

test("a worker paused at a limit or its threshold says which window, and when it starts over", () => {
  assert.equal(pauseWords(agent(), NOW), null);
  const limit = agent({ pause: { reason: "limit", window: "five_hour", label: "Session", until: NOW + 2 * 3_600_000 + 60_000 } });
  assert.equal(pauseWords(limit, NOW), "paused at its session limit, starts over in 2h");
  const threshold = agent({ pause_at: 80, pause: { reason: "threshold", window: "seven_day", label: "Weekly" } });
  assert.equal(pauseWords(threshold, NOW), "paused at its threshold (80%), its weekly window");
  assert.equal(pauseWords(agent({ pause: { reason: "limit" } }), NOW), "paused at its usage limit");
});

test("a worker's threshold is said with kiln's rule for it", () => {
  assert.equal(thresholdWords(track("T2", 3)), null, "no worker");
  assert.equal(thresholdWords(track("T7", 4, { worker: agent({ pause_at: 95 }), wants: 95 })), "pauses at 95%");
  assert.equal(thresholdWords(track("T10", 2, { worker: agent({ pause_at: 80 }), wants: 80 })), "pauses at 80%: the lowest-weight track at work");
  assert.equal(thresholdWords(track("T6", 3, { worker: agent({ pause_at: 90 }), wants: 95 })), "pauses at 90% (kiln's rule: 95%)", "a person's");
  assert.equal(thresholdWords(track("T1", 5, { worker: agent(), wants: 95 })), "kiln sets it to pause at 95%", "not set yet");
});

test("a goal handed to the worker says it's under way there; one to come, next or held", () => {
  assert.equal(goalWords({ status: "pending", on: "01a15c10306176b4", on_name: "Hex T7" }, "running"), "Under way on Hex T7");
  assert.equal(goalWords({ status: "pending", on: "01a15c10306176b4" }, "paused"), "Under way on 01a15c10");
  assert.equal(goalWords({ status: "active" }, "running"), "Under way", "the worker's own");
  assert.equal(goalWords({ status: "pending" }, "next"), "Next");
  assert.equal(goalWords({ status: "pending" }, "held"), "Next, held");
});

test("whether the coordinator was told of the settings is said: delivered, or why not yet", () => {
  assert.equal(noticeWords({}, NOW), null);
  const told = noticeWords({ notice: { text: "slots 6 → 5", at: NOW - 120_000, delivered: true, message: "m1" } }, NOW);
  assert.equal(told?.tone, "ok");
  assert.match(told?.text ?? "", /^The coordinator was told 2m ago: kiln queued the message for its session/);
  assert.match(noticeWords({ notice: { text: "", at: NOW + 20_000, delivered: true } }, NOW)?.text ?? "", /^The coordinator was told now:/, "a clock read before it went");
  const not = noticeWords({ notice: { text: "slots 6 → 5", at: NOW, delivered: false, why: "the coordinator's machine isn't connected right now" } }, NOW);
  assert.deepEqual(not, { text: "Not yet delivered to the coordinator: the coordinator's machine isn't connected right now. kiln tries again every 15 s.", tone: "warn" });
});

test("a change request says how far it went, built, merged or closed, and its checks", () => {
  assert.equal(changeWords({ title: "t", url: "u", branch: "cp/t7-hex", change: "CR47", state: "merged", checks: "passed" }), "CR47 · merged · checks passed");
  assert.equal(changeWords({ title: "t", url: "u", branch: "cp/t3-journal", state: "built" }), "cp/t3-journal · built");
  assert.equal(passWords(0.25), "0.25");
  assert.equal(passWords(2), "2");
});

test("the form as edited: only what changed is sent, a worker changed or taken off, and what's out of bounds is said", () => {
  const p = { slots: 6, tracks: [track("T1", 5, { assigned: "a1" }), track("T2", 3)] };
  assert.deepEqual(edited(p, "6", { T1: "5", T2: "3" }), { body: {}, assign: {}, error: null, changed: false });
  assert.deepEqual(edited(p, "5", { T1: "5", T2: "4" }), { body: { slots: 5, weights: { T2: 4 } }, assign: {}, error: null, changed: true });
  assert.deepEqual(edited(p, "6", {}, { T1: "a1", T2: "b2" }), { body: {}, assign: { T2: "b2" }, error: null, changed: true }, "T1's as it was");
  assert.deepEqual(edited(p, "6", {}, { T1: "" }).assign, { T1: null }, "no worker");
  assert.equal(edited(p, "0", {}).error, "Slots is a whole number from 1 to 32");
  assert.equal(edited(p, "6", { T1: "101" }).error, "T1's weight is a whole number from 1 to 100");
  assert.equal(edited(p, "6", { T1: "2.5" }).changed, false);
});
