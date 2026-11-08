import assert from "node:assert/strict";
import { test } from "node:test";
import { lineDiff } from "../src/lib/line-diff.ts";

test("w29_r22_an_update_drafts_changed_lines", () => {
  const was = [
    "jobs:",
    "  rust:",
    "    uses: kit/rust-check.yml@v2.3.0",
    "    with: { toolchain: stable }",
    "  bench:",
    "    runs-on: [bench]",
    "    steps: []",
    "  docs:",
    "    runs-on: [docs]",
  ].join("\n");
  const now = was.replace("@v2.3.0", "@v2.4.0");
  const d = lineDiff(was, now, 1);
  assert.deepEqual(
    d.map((l) => [l.kind, l.old, l.now, l.text]),
    [
      ["gap", null, null, ""],
      ["same", 2, 2, "  rust:"],
      ["del", 3, null, "    uses: kit/rust-check.yml@v2.3.0"],
      ["add", null, 3, "    uses: kit/rust-check.yml@v2.4.0"],
      ["same", 4, 4, "    with: { toolchain: stable }"],
      ["gap", null, null, ""],
    ],
  );
  // Nothing changed: one fold; all new: every line put in.
  assert.deepEqual(
    lineDiff("a\nb", "a\nb").map((l) => l.kind),
    ["gap"],
  );
  assert.deepEqual(
    lineDiff("", "x\ny", 0).map((l) => [l.kind, l.text]),
    [
      ["del", ""],
      ["add", "x"],
      ["add", "y"],
    ],
  );
});
