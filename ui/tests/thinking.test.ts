import assert from "node:assert/strict";
import { test } from "node:test";
import type { HEvent } from "../src/api/harness.ts";
import { turnsOf, type ToolEvent } from "../src/views/harness/derive.ts";
import { latestThought, thinkingStarted, thinkingWords, thoughtIsLive, type ThoughtItem } from "../src/views/harness/thinking.ts";

const prompt: HEvent = { id: "prompt", seq: 0, at: 1000, kind: "user", text: "Take a look" };
const thought = (id: string, at: number, streaming = true): Extract<HEvent, { kind: "thinking" }> => ({ id, seq: at, at, kind: "thinking", text: id, streaming });
const call = (id: string, started: number, ended?: number): ToolEvent => ({
  id, seq: started, at: started, kind: "tool", tool_id: id, name: "shell", tool: "shell", title: id,
  input: {}, status: "done", started, ended,
});
const turn = (...events: HEvent[]) => turnsOf([prompt, ...events], undefined, true)[0]!;

test("thinking words progress at 30 and 60 seconds of the current turn", () => {
  for (const seconds of [0, 1, 29, 29.999]) assert.equal(thinkingWords(seconds), "Thinking");
  for (const seconds of [30, 45, 59, 59.999]) assert.equal(thinkingWords(seconds), "Thinking deeply");
  for (const seconds of [60, 90, 3600]) assert.equal(thinkingWords(seconds), "Thinking very deeply");
});

test("tool calls accumulate elapsed time across the same turn", () => {
  const first = turn(thought("first", 2000, false), call("one", 3000, 123000));
  const second = turn(thought("first", 2000, false), call("one", 3000, 123000), call("two", 130000, 131000));
  assert.equal(thinkingStarted(first), prompt.at);
  assert.equal(thinkingStarted(second), prompt.at);
  assert.equal(thinkingWords((132000 - thinkingStarted(second)) / 1000), "Thinking very deeply");
});

test("grouped thoughts and their quiet tail share the turn clock", () => {
  const current = turn(thought("old", 2000, false), thought("new", 90000));
  const item = current.items[0] as ThoughtItem;
  assert.equal(latestThought(item).id, "new");
  assert.equal(thoughtIsLive(item, true), true);
  assert.equal(thinkingStarted(current), prompt.at);
  const settled = turn(thought("old", 2000, false), thought("new", 90000, false));
  assert.equal(thinkingStarted(settled), thinkingStarted(current));
});

test("replies and legacy tool records keep the same clock after reconnect", () => {
  const reply: HEvent = { id: "reply", seq: 3, at: 3000, kind: "assistant", text: "Working on it", streaming: false };
  assert.equal(thinkingStarted(turn(thought("first", 2000, false), reply)), prompt.at);
  assert.equal(thinkingStarted(turn(call("legacy", 2000))), prompt.at);
  const next = turn(reply, thought("next", 90000));
  assert.equal(thinkingStarted(next, 1500), 1500);
  assert.equal(thinkingStarted(structuredClone(next), 1500), 1500);
});

test("a genuinely new harness turn resets the clock and ignores stale streaming flags", () => {
  const resumed = turn(thought("old", 2000));
  const item = resumed.items[0] as ThoughtItem;
  assert.equal(thinkingStarted(resumed, 100000), 100000);
  assert.equal(thoughtIsLive(item, true, 100000), false);
  assert.equal(thoughtIsLive(item, false), false);
  assert.equal(thinkingStarted(turn(), 100000), 100000);
});

test("notices and steering do not reset an active harness turn", () => {
  const notice: HEvent = { id: "notice", seq: 3, at: 50000, kind: "notice", level: "info", text: "Still working" };
  assert.equal(thinkingStarted(turn(call("one", 2000, 3000), notice)), prompt.at);
  const turns = turnsOf([prompt, thought("old", 2000, false), { ...prompt, id: "new-prompt", at: 100000 }], undefined, true);
  assert.equal(thinkingStarted(turns[1]!, 1500), 1500, "steering preserves the active turn's start");
  assert.equal(thinkingStarted(turns[1]!), 100000, "without live state the new prompt is the fallback");
});
