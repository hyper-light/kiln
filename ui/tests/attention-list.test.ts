/** Mission control's list kept for you, plan 01a199e1b177752e84dad5dc990e6a15: its sections and the Undo where a row was. */
import assert from "node:assert/strict";
import { registerHooks } from "node:module";
import { test } from "node:test";
import type { UndoState } from "../src/views/harness/attention-list.ts";
import { defaultInfo } from "../mock/defaults.ts";
import type { MissionRow } from "../src/views/harness/mission-rows.ts";

// The list's sections come from its sort (list-tools.tsx), whose view imports need no CSS in Node.
const loader = registerHooks({
  load(url, context, nextLoad) {
    return url.endsWith(".css") ? { format: "module", source: "export {};", shortCircuit: true } : nextLoad(url, context);
  },
});
const { described, entriesOf, inverseOf, withUndo } = await import("../src/views/harness/attention-list.ts");
loader.deregister();

const row = (id: string, more: Partial<MissionRow> = {}): MissionRow => ({ a: { ...defaultInfo(), id, approvals: [] }, stand: "working", ...more });
const keys = (rows: { key: string }[]) => rows.map((e) => e.key);
const undo = (more: Partial<UndoState>): UndoState => ({ list: "live", prev: null, next: null, height: 84, seq: 1, visit: "v", shown: "", text: "", pending: false, ...more });

test("mc_attention: pins are a section of their own under Needs you, in your order, then the rest", () => {
  const rows = [row("ask", { stand: "needs" }), row("p1", { pinned: 1 }), row("p2", { pinned: 2 }), row("rest")];
  assert.deepEqual(keys(entriesOf(rows, true)), ["row:ask", "sep:pinned", "row:p1", "row:p2", "sep:rest", "row:rest"]);
  assert.equal(entriesOf(rows, true).find((e) => e.key === "sep:pinned")?.pinned, 2);
  assert.deepEqual(keys(entriesOf(rows.slice(1, 3), true)), ["sep:pinned", "row:p1", "row:p2"], "no rest: nothing after them");
  assert.deepEqual(keys(entriesOf(rows, false)), ["row:ask", "row:p1", "row:p2", "row:rest"], "a sort chosen on purpose has no sections");
  assert.deepEqual(keys(entriesOf([row("pinned-ask", { stand: "needs", pinned: 1 }), row("rest")], true)), ["row:pinned-ask", "row:rest"], "a pin that needs you is in Needs you");
});

test("mc_attention: the Undo stands where the row was, nothing around it moving", () => {
  const after = entriesOf([row("a"), row("b"), row("d")], true);
  // c was parked from between b and d: its Undo is there.
  assert.deepEqual(keys(withUndo(after, undo({ prev: "row:b", next: "row:d" }), "live")), ["row:a", "row:b", "undo", "row:d"]);
  // The first row: at the top.
  assert.deepEqual(keys(withUndo(after, undo({ prev: null, next: "row:a" }), "live")), ["undo", "row:a", "row:b", "row:d"]);
  // What was before it went too (parked with it): before what was after it.
  assert.deepEqual(keys(withUndo(after, undo({ prev: "row:gone", next: "row:d" }), "live")), ["row:a", "row:b", "undo", "row:d"]);
  // Pinned from the rest's first place: in the rest, not among the pins.
  const pinned = entriesOf([row("p", { pinned: 1 }), row("x", { pinned: 2 }), row("r2")], true);
  assert.deepEqual(keys(withUndo(pinned, undo({ prev: "sep:rest", next: "row:r2" }), "live")), ["sep:pinned", "row:p", "row:x", "sep:rest", "undo", "row:r2"]);
  assert.deepEqual(keys(withUndo(after, undo({ list: "parked", prev: "row:a" }), "live")), keys(after), "another list's Undo isn't here");
});

test("mc_attention: what an action did, in words, and what undoes it", () => {
  const now = new Date(2026, 10, 1, 12).getTime();
  const name = (id: string) => (id === "a" ? "Update the docs" : id);
  assert.match(described([{ action: "park", agent: "a", until: new Date(2026, 10, 2, 9).getTime() }], name, now)?.done ?? "", /^“Update the docs” parked until tomorrow at /);
  assert.equal(
    described(
      ["a", "b", "c"].map((agent) => ({ action: "archive", agent })),
      name,
      now,
    )?.done,
    "3 agents archived",
  );
  assert.equal(described([{ action: "seen", agent: "a" }], name, now), null, "clearing Updated has no Undo");
  assert.equal(described([{ action: "reorder", order: ["a"] }], name, now), null, "nor does moving a pin");
  assert.deepEqual(
    inverseOf(
      [
        { action: "park", agent: "a", until: 1 },
        { action: "park", agent: "b", until: 1 },
      ],
      [
        { agent: "a", revision: 3 },
        { agent: "b", revision: 5 },
      ],
      new Set(),
    ),
    [
      { action: "undo", agent: "a", revision: 3 },
      { action: "undo", agent: "b", revision: 5 },
    ],
    "one Undo for the batch: each park's own",
  );
  assert.deepEqual(
    inverseOf(
      [
        { action: "archive", agent: "a" },
        { action: "archive", agent: "b" },
      ],
      [],
      new Set(["b"]),
    ),
    [{ action: "restore", agent: "a" }],
    "an archive is undone by Restore, of those that went",
  );
  assert.deepEqual(inverseOf([{ action: "restore", agent: "a" }], [], new Set()), [{ action: "archive", agent: "a" }]);
});
