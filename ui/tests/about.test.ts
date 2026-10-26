import assert from "node:assert/strict";
import { registerHooks } from "node:module";
import { test } from "node:test";

// The card's own logic, without its CSS (Node has none to load).
const loader = registerHooks({
  load(url, context, nextLoad) {
    return url.endsWith(".css") ? { format: "module", source: "export {};", shortCircuit: true } : nextLoad(url, context);
  },
});
const { PARTS, partsOf, wordAt } = await import("../src/components/code/about.tsx");
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
