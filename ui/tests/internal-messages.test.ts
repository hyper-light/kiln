import assert from "node:assert/strict";
import { test } from "node:test";
import type { HEvent } from "../src/api/harness.ts";
import { isInternal, turnsOf } from "../src/views/harness/derive.ts";

const user = (id: string, text: string, client_id?: string): HEvent => ({ id, seq: 0, at: 0, kind: "user", text, ...(client_id ? { client_id } : {}) });
const said = (id: string, text: string): HEvent => ({ id, seq: 0, at: 0, kind: "assistant", text });
const tool = (id: string): HEvent => ({ id, seq: 0, at: 0, kind: "tool", tool_id: id, name: "Bash", tool: "shell", title: "ls", input: {}, status: "done", started: 0 });

test("kiln's news and other agents' messages are internal; a person's aren't", () => {
  assert.ok(isInternal("news-01a1d07d0bff"));
  assert.ok(isInternal("from-agent-01a1d07d0bff71179835e1526868a53a-01a1d0b4"));
  assert.ok(!isInternal("01a1d0b4c3f5"));
  assert.ok(!isInternal("kiln-keep-1"));
  assert.ok(!isInternal(undefined));
});

test("news after the agent answered starts a turn that shows no prompt, only the agent's work", () => {
  const turns = turnsOf([user("u1", "go"), said("a1", "started them"), user("n1", "Agents you started: …", "news-1"), tool("t1"), said("a2", "all green")]);
  assert.equal(turns.length, 2);
  assert.equal(turns[1]!.prompt, null);
  assert.deepEqual(turns[1]!.hidden, ["n1"]);
  assert.equal(turns[1]!.outcome?.text, "all green");
  assert.ok(turns.every((t) => t.prompt?.client_id === undefined || !isInternal(t.prompt.client_id)));
});

test("a peer's message steered into the agent's work stays part of that turn, unshown", () => {
  const turns = turnsOf([user("u1", "go"), tool("t1"), user("m1", "Which commit has the fix?", "from-agent-01a1peer-1"), tool("t2"), said("a1", "done")]);
  assert.equal(turns.length, 1);
  assert.equal(turns[0]!.prompt?.id, "u1");
  assert.deepEqual(turns[0]!.hidden, ["m1"]);
});
