import assert from "node:assert/strict";
import { test } from "node:test";
import { arcPath, GLYPHS, journeyLabel, moves, pipDone, pipFlows, signature, STATES, type Pip } from "../src/components/ui/status-glyph-spec.ts";

test("every state has a word and a shape no other state has, so none needs its colour to be told apart", () => {
  const seen = new Map<string, string>();
  for (const state of STATES) {
    assert.ok(GLYPHS[state].word.length > 0, `${state} has no word`);
    const shape = signature(state);
    assert.equal(seen.get(shape), undefined, `${state} has the same shape as ${seen.get(shape)}`);
    seen.set(shape, state);
  }
});

test("a code failure, CI's own error and a flake are three different glyphs and words", () => {
  const words = new Set(["failed", "error", "flaky"].map((s) => GLYPHS[s as "failed"].word));
  assert.equal(words.size, 3);
  assert.notEqual(signature("failed"), signature("error"));
  assert.notEqual(signature("failed"), signature("flaky"));
});

test("every state but 'not yet' and a person has a motion of its own", () => {
  for (const state of STATES) assert.equal(moves(state), state !== "none" && state !== "person", state);
});

test("a wait's arc grows clockwise from the top as it nears its end", () => {
  assert.match(arcPath(0.25), /^M7 2 A5 5 0 0 1 12\.00 7\.00$/);
  assert.match(arcPath(0.75), /^M7 2 A5 5 0 1 1 2\.00 7\.00$/);
  assert.match(arcPath(Number.NaN), /^M7 2 A5 5 0 0 1 /);
});

test("the journey strip's accessible name lists each stage and how it stands", () => {
  const pips: Pip[] = [
    { stage: "land", state: "passed" },
    { stage: "checks", state: "flaky" },
    { stage: "build", state: "passed" },
    { stage: "staging", state: "healthy" },
    { stage: "production", state: "needs" },
    { stage: "replant", state: "none" },
  ];
  assert.equal(journeyLabel(pips), "Land: passed, Checks: flaky, Build: passed, Staging: healthy, Production: needs you, Replant: not yet");
});

test("the line into a stage lights once the stage before is done, and light flows where work is under way", () => {
  assert.equal(pipDone("passed"), true);
  assert.equal(pipDone("running"), false);
  assert.equal(pipFlows("running"), true);
  assert.equal(pipFlows("needs"), true);
  assert.equal(pipFlows("passed"), false);
});
