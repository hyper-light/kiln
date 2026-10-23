import assert from "node:assert/strict";
import { test } from "node:test";
import { blocked, curve, effectiveNeeds, headline, inlineParts, rounds, type TitlePart } from "../src/views/plans/graph-layout.ts";
import { planSteps, stepMarkdown } from "../src/views/plans/steps.ts";

const step = (n: number, needs: number[]) => ({ n, title: `s${n}`, needs });

test("steps fall in rounds after the latest step they wait for, side by side within one", () => {
  const graph = [step(1, []), step(2, [1]), step(3, [1]), step(4, []), step(5, [2, 3])];
  assert.deepEqual(
    rounds(graph).map((r) => r.map((s) => s.n)),
    [[1, 4], [2, 3], [5]],
  );
  // A chain, one a round; a step that waits for one not there waits for nothing.
  assert.deepEqual(
    rounds([step(1, []), step(2, [1]), step(3, [2])]).map((r) => r.map((s) => s.n)),
    [[1], [2], [3]],
  );
  assert.deepEqual(
    rounds([step(1, [9])]).map((r) => r.map((s) => s.n)),
    [[1]],
  );
});

test("a plan whose steps say nothing of it runs each after the one before", () => {
  assert.deepEqual(effectiveNeeds([undefined, undefined, []]), [[], [1], [2]]);
  assert.deepEqual(effectiveNeeds([[], [1], [1]]), [[], [1], [1]]);
  assert.deepEqual(effectiveNeeds([undefined, [1], undefined]), [[], [1], []], "once one says, each says for itself");
});

test("a line that would run behind another step isn't drawn: that step says what it waits for", () => {
  // A phone's rows of two steps: 1 and 2, then 3 and 4, then 5. A line from 1 down to 5 runs behind 3.
  const at = (left: number, top: number) => ({ left, top, right: left + 150, bottom: top + 50 });
  const [one, three, five] = [at(0, 0), at(0, 70), at(0, 140)];
  const mid = (b: { left: number; right: number }) => (b.left + b.right) / 2;
  assert.equal(blocked(curve(mid(one), one.bottom, mid(five), five.top, false), [three]), true);
  // Down to the step right under it: nothing in between.
  assert.equal(blocked(curve(mid(one), one.bottom, mid(three), three.top, false), [five]), false);
  // Across to the next round, past a step above the line: drawn.
  const [a, b, above] = [at(0, 100), at(200, 100), at(100, 0)];
  assert.equal(blocked(curve(a.right, 125, b.left, 125, true), [above]), false);
  // The ends of a line touch its own steps' edges, never another's inside.
  assert.deepEqual(curve(0, 0, 10, 20, false, 2)[0], [0, 0]);
  assert.deepEqual(curve(0, 0, 10, 20, false, 2)[2], [10, 20]);
});

const said = (parts: TitlePart[]) => parts.map((p) => (p.code ? `<${p.text}>` : p.text)).join("");

test("a step's markdown reads as its words, its code as code", () => {
  assert.deepEqual(inlineParts("Run **all** of `cargo test` per [the docs](https://x.y)"), [
    { code: false, text: "Run all of " },
    { code: true, text: "cargo test" },
    { code: false, text: " per the docs" },
  ]);
});

test("a step that's a paragraph is titled by its bold lead, its first sentence, or what's before its colon", () => {
  // A bold lead names it.
  assert.equal(said(headline("**An interest word** (`crates/mem/src/handle.rs`, beside `Encoded`'s layout): - Layout: the real shard id")), "An interest word");
  // A short first sentence is its title, without its full stop.
  assert.equal(said(headline("Reproduce it: the CLI test fails 3 of 3. Then fix the daemon.")), "Reproduce it: the CLI test fails 3 of 3");
  // A long one, cut at its colon (never one inside code).
  const long =
    "`crates/rt/tests/readiness.rs`: `a_task_racing_two_descriptors_learns_which_one_became_readable` (`left: Quiet`, `right: Signalled`), and `a_deadline_on_a_readiness_wait_that_never_comes_is_reported_as_the_deadline`.";
  assert.equal(said(headline(long)), "<crates/rt/tests/readiness.rs>");
  // A short title is itself; code beside code keeps the space between them.
  assert.equal(said(headline("Op: conversations.search")), "Op: conversations.search");
  assert.equal(said(headline("`a.rs` `b.rs` together")), "<a.rs> <b.rs> together");
});

test("a long title without a place to cut stops between words", () => {
  const words = Array.from({ length: 40 }, (_, i) => `word${i}`).join(" ");
  const t = said(headline(words, 60));
  assert.ok(t.endsWith("…"), t);
  assert.ok(t.length <= 60, t);
  assert.ok(/word\d+…$/.test(t), t);
});

test("a plain plan's step reads as its markdown, what's indented under it and all", () => {
  const body = [
    "# Fix it",
    "",
    "## Tests first",
    "",
    "1. `readiness.rs`: two descriptors race.",
    "",
    "```",
    "- not a step: code",
    "```",
    "",
    "## The change",
    "",
    "2. **An interest word** (`handle.rs`):",
    "   - Layout: the real shard id.",
    "   - Decoding it never yields a handle.",
    "",
    "   ```rust",
    "   const MARK: u16 = 0x7fff;",
    "   ```",
    "3. Run the tests.",
    "",
    "Then ship it.",
  ].join("\n");
  // Numbered as the server numbers them: list items at the top, none inside code.
  assert.equal(planSteps(body).length, 3);
  assert.equal(stepMarkdown(body, 1), "`readiness.rs`: two descriptors race.");
  assert.equal(stepMarkdown(body, 2), "**An interest word** (`handle.rs`):\n- Layout: the real shard id.\n- Decoding it never yields a handle.\n\n```rust\nconst MARK: u16 = 0x7fff;\n```");
  assert.equal(stepMarkdown(body, 3), "Run the tests.");
  assert.equal(stepMarkdown(body, 4), "");
});
