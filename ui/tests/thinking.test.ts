import assert from "node:assert/strict";
import { test } from "node:test";
import type { HEvent } from "../src/api/harness.ts";
import { turnsOf, type ToolEvent } from "../src/views/harness/derive.ts";
import { latestThought, quietThinkingPhase, thinkingWords, thoughtIsLive, type ThoughtItem } from "../src/views/harness/thinking.ts";

const prompt: HEvent = { id: "prompt", seq: 0, at: 1000, kind: "user", text: "Take a look" };
const thought = (id: string, at: number, streaming = true): Extract<HEvent, { kind: "thinking" }> => ({ id, seq: at, at, kind: "thinking", text: id, streaming });
const call = (id: string, started: number, ended?: number): ToolEvent => ({
  id, seq: started, at: started, kind: "tool", tool_id: id, name: "shell", tool: "shell", title: id,
  input: {}, status: "done", started, ended,
});
const turn = (...events: HEvent[]) => turnsOf([prompt, ...events], undefined, true)[0]!;

test("thinking words progress at 30 and 60 seconds of the current phase", () => {
  for (const seconds of [0, 1, 29, 29.999]) assert.equal(thinkingWords(seconds), "Thinking");
  for (const seconds of [30, 45, 59, 59.999]) assert.equal(thinkingWords(seconds), "Thinking deeply");
  for (const seconds of [60, 90, 3600]) assert.equal(thinkingWords(seconds), "Thinking very deeply");
});

test("a tool completion resets quiet thinking without charging the tool's running time", () => {
  const first = turn(thought("first", 2000, false), call("one", 3000, 123000));
  assert.deepEqual(quietThinkingPhase(first), { key: "one", since: 123000 });
  const second = turn(thought("first", 2000, false), call("one", 3000, 123000), call("two", 130000, 131000));
  assert.deepEqual(quietThinkingPhase(second), { key: "one", since: 131000 });
  assert.equal(thinkingWords((132000 - quietThinkingPhase(second).since!) / 1000), "Thinking");
});

test("the newest grouped thought starts fresh and its quiet tail keeps the same clock", () => {
  const current = turn(thought("old", 2000, false), thought("new", 90000));
  const item = current.items[0] as ThoughtItem;
  assert.equal(latestThought(item).id, "new");
  assert.equal(thoughtIsLive(item, true), true);
  assert.deepEqual(quietThinkingPhase(current), { key: "old", since: 90000 });
  assert.equal(thinkingWords((91000 - latestThought(item).at) / 1000), "Thinking");
  const settled = turn(thought("old", 2000, false), thought("new", 90000, false));
  assert.deepEqual(quietThinkingPhase(settled), { key: "old", since: 90000 });
});

test("a reply or a legacy tool with no completion time starts an observed quiet phase", () => {
  const reply: HEvent = { id: "reply", seq: 3, at: 3000, kind: "assistant", text: "Working on it", streaming: false };
  assert.deepEqual(quietThinkingPhase(turn(thought("first", 2000, false), reply)), { key: "reply" });
  assert.deepEqual(quietThinkingPhase(turn(call("legacy", 2000))), { key: "legacy" });
  const next = turn(reply, thought("next", 90000));
  assert.equal(quietThinkingPhase(next).since, 90000);
});

test("a resumed turn uses its own start and ignores stale streaming flags", () => {
  const resumed = turn(thought("old", 2000));
  const item = resumed.items[0] as ThoughtItem;
  assert.deepEqual(quietThinkingPhase(resumed, 100000), { key: "turn:prompt:100000", since: 100000 });
  assert.equal(thoughtIsLive(item, true, 100000), false);
  assert.equal(thoughtIsLive(item, false), false);
  assert.equal(quietThinkingPhase(turn(), 100000).since, 100000);
});

test("notices do not reset thinking and a newer prompt still starts a new phase", () => {
  const notice: HEvent = { id: "notice", seq: 3, at: 50000, kind: "notice", level: "info", text: "Still working" };
  assert.equal(quietThinkingPhase(turn(call("one", 2000, 3000), notice)).since, 3000);
  const turns = turnsOf([prompt, thought("old", 2000, false), { ...prompt, id: "new-prompt", at: 100000 }], undefined, true);
  assert.equal(quietThinkingPhase(turns[1]!).since, 100000);
});
