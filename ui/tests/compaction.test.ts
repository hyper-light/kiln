import assert from "node:assert/strict";
import { test } from "node:test";
import type { HEvent } from "../src/api/harness.ts";
import { isCompaction, turnsOf } from "../src/views/harness/derive.ts";

const at = (seq: number) => ({ seq, at: 1000 + seq * 10 });
const notice = (seq: number, id: string, text: string, level: "info" | "warn" = "info"): HEvent => ({ id, ...at(seq), kind: "notice", level, text });

test("a compaction's marker is the boundary where it ran, not a notice, and leaves the turn its answer", () => {
  const events: HEvent[] = [
    { id: "u1", ...at(1), kind: "user", text: "keep going" },
    { id: "t1", ...at(2), kind: "tool", tool_id: "t1", name: "Bash", tool: "shell", title: "cargo test", input: {}, status: "done", started: 1020 },
    notice(3, "compaction:r-1", "Compacted · 967k → 48k tokens · 1m 38s"),
    { id: "a1", ...at(4), kind: "assistant", text: "Done." },
    notice(5, "compaction:r-2", "Couldn't compact: prompt too long", "warn"),
  ];
  const [turn] = turnsOf(events);
  assert.deepEqual(
    turn?.items.map((i) => i.type),
    ["step", "compaction", "compaction"],
  );
  assert.equal(turn?.outcome?.id, "a1");
});

test("what kiln said of a compaction before its markers had ids is one too; other notices aren't", () => {
  assert.ok(isCompaction({ id: "notice-x-3", ...at(1), kind: "notice", level: "info", text: "Conversation compacted (was 967k tokens)" }));
  assert.ok(isCompaction({ id: "compaction:stopped-9", ...at(1), kind: "notice", level: "warn", text: "Compaction stopped after 41s" }));
  assert.ok(!isCompaction({ id: "notice-x-4", ...at(1), kind: "notice", level: "info", text: "Interrupted" }));
  // Two in a row are two boundaries, not one notice said twice.
  const [turn] = turnsOf([notice(1, "notice-a", "Conversation compacted"), notice(2, "notice-b", "Conversation compacted")]);
  assert.deepEqual(
    turn?.items.map((i) => i.type),
    ["compaction", "compaction"],
  );
});
