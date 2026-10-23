import assert from "node:assert/strict";
import { test } from "node:test";
import { researchCall } from "../src/api/research.ts";

test("kiln's research tools, as an MCP call (Claude Code, Codex), name the research they touch", () => {
  // Adding to it names it.
  assert.deepEqual(researchCall({ server: "kiln", tool_short: "research_add", title: "research_add", input: { id: "rs_1", findings: [] } }), { verb: "add", id: "rs_1" });
  // Starting it: its answer is the research, its id first.
  assert.deepEqual(researchCall({ server: "kiln", tool_short: "research_start", input: { question: "Why?" }, output: '{\n  "id": "rs_2",\n  "question": "Why?"\n}' }), { verb: "start", id: "rs_2" });
  // Listing it touches none in particular; other tools aren't research.
  assert.equal(researchCall({ server: "kiln", tool_short: "research_list", input: {} }), null);
  assert.equal(researchCall({ server: "kiln", tool_short: "goals_done", input: { id: "g1" } }), null);
  assert.equal(researchCall({ server: "github", tool_short: "research_add", input: { id: "x" } }), null);
});

test("an ACP agent's research call (Devin), reported in words, its input as text and its answer quoted", () => {
  const added = researchCall({
    title: "Called research_add from kiln",
    input: '{"findings": [{"claim": "Devin exposes effort over ACP", "cites": []}]}',
    output: '"{\\n  \\"id\\": \\"01a1663976537285a6adec1dc24996e1\\",\\n  \\"question\\": \\"Devin effort can be chosen\\"\\n}"',
  });
  assert.deepEqual(added, { verb: "add", id: "01a1663976537285a6adec1dc24996e1" });
  // Naming it in its input, that's the one.
  assert.deepEqual(researchCall({ title: "Called research_get from kiln", input: '{"id": "rs_9"}' }), { verb: "get", id: "rs_9" });
  // Not yet answered, and naming none: research, its id not known yet.
  assert.deepEqual(researchCall({ title: "Called research_start from kiln", input: '{"question": "Why?"}' }), { verb: "start", id: null });
  assert.equal(researchCall({ title: "Called goals_done from kiln", input: "{}" }), null);
});
