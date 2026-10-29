import assert from "node:assert/strict";
import { registerHooks } from "node:module";
import { test } from "node:test";
import type { HEvent } from "../src/api/harness.ts";

// The card's own logic, without its CSS (Node has none to load).
const loader = registerHooks({
  load(url, context, nextLoad) {
    return url.endsWith(".css") ? { format: "module", source: "export {};", shortCircuit: true } : nextLoad(url, context);
  },
});
const { PARTS, linesLabel, partsOf, replyTo, wordAt } = await import("../src/components/code/about.tsx");
loader.deregister();

test("a tap picks the name under it, or nothing on space and punctuation", () => {
  const line = "let wait = backoff.next(now).min(deadline - now);";
  const at = (s: string, k = 0) => line.indexOf(s) + k;
  assert.deepEqual(wordAt(line, at("next", 2)), { start: at("next"), end: at("next") + 4 });
  // Just past a name's end still picks it (a tap lands between letters).
  assert.deepEqual(wordAt(line, at("backoff", 7)), { start: at("backoff"), end: at("backoff") + 7 });
  assert.equal(wordAt(line, at("= ", 1)), null);
  assert.equal(wordAt(line, -1), null);
  assert.deepEqual(wordAt("$el_2 + x", 1), { start: 0, end: 5 });
  assert.deepEqual(wordAt("naïve(x)", 2), { start: 0, end: 5 });
});

test("an answer's parts come by their headings, as they stream", () => {
  assert.deepEqual(PARTS, ["What it is", "What it does here", "Used elsewhere", "Worth knowing"]);
  // Half way: the first two parts are here, the others not yet.
  const half = "## What it is\nA backoff.\n\n## What it does here\nPicks the sleep";
  assert.deepEqual(partsOf(half), ["A backoff.", "Picks the sleep", null, null]);
  const all = "### **What it is**\nx\n## Used elsewhere\n- `a.rs:1` `b()`\n## Worth knowing:\nz";
  assert.deepEqual(partsOf(all), ["x", null, "- `a.rs:1` `b()`", "z"]);
  // An answer that ignored the headings is all its first part.
  assert.deepEqual(partsOf("It's a retry loop."), ["It's a retry loop.", null, null, null]);
  assert.deepEqual(partsOf(""), [null, null, null, null]);
});

test("lines asked about are named as one stretch, whichever was tapped first", () => {
  assert.equal(linesLabel(44, 44), "line 44");
  assert.equal(linesLabel(48, 44), "lines 44–48");
  assert.equal(linesLabel(44, undefined), "line 44");
});

test("an agent answering itself: its words after the question, not its subagents', until the next message", () => {
  const ev = (e: Record<string, unknown>, i: number) => ({ seq: i, at: i, ...e }) as unknown as HEvent;
  const events = [
    { id: "u0", kind: "user", text: "earlier" },
    { id: "a0", kind: "assistant", text: "earlier answer" },
    { id: "u1", kind: "user", text: "what is next?", client_id: "msg-1" },
    { id: "a1", kind: "assistant", text: "## What it is\nA backoff." },
    { id: "s1", kind: "assistant", text: "a subagent's words", parent: "task-1" },
    { id: "a2", kind: "assistant", text: "## Worth knowing\nIt counts." },
    { id: "u2", kind: "user", text: "next question" },
    { id: "a3", kind: "assistant", text: "not this" },
  ].map(ev);
  assert.equal(replyTo(events, "msg-1"), "## What it is\nA backoff.\n\n## Worth knowing\nIt counts.");
  // Its own id finds it too; before it shows, there's nothing yet; asked but not answered, nothing said.
  assert.equal(replyTo(events, "u2"), "not this");
  assert.equal(replyTo(events, "msg-9"), null);
  assert.equal(replyTo(events.slice(0, 3), "msg-1"), "");
});
