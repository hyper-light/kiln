// The origin's check (docs/browser-lab.md, "The baselines"; plan 01a1a832e9cc step 5): what it
// reads back off each carrier, and what counts as the same frame.
import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, writeFileSync, rmSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { createHash } from "node:crypto";
import { sseEvents, lines, holds, check } from "../check-origin.mjs";
import { normal } from "../catalogue.mjs";

test("g1_s5 server-sent events read as a browser reads them", () => {
  assert.deepEqual(sseEvents('data: {"a":1}\n\ndata: two\ndata: lines\n\n: comment\n\ndata: cut'), ['{"a":1}', "two\nlines"]);
  assert.deepEqual(lines("a\nb\nc"), ["a", "b"]);
});

test("g1_s5 a replayed frame holds with a new stamp, and a changed or missing one doesn't", () => {
  const sent = ['{"text":"@lab:1793836525502235 hi"}', "line one\r\n"];
  assert.deepEqual(holds("terminal", sent, ['{"text":"@lab:1793839999999123 hi"}', "line one"]), []);
  assert.equal(holds("session", sent, ['{"text":"@lab:1793839999999123 ho"}']).length, 2);
});

test("g1_s5 the origin replays every recorded frame on both carriers", async () => {
  const dir = mkdtempSync(join(tmpdir(), "kiln-lab-origin-test-"));
  try {
    const frames = ['{"seq":1,"text":"@lab:1793836525502235 a"}', '{"seq":2,"text":"@lab:1793836525502236 b"}'];
    const term = ["$ ls\r\n", "a b c\r\n"];
    writeFileSync(join(dir, "replay.json"), JSON.stringify({ streams: { session: frames, terminal: term }, writes: {} }));
    const digest = (f) => createHash("sha256").update(f.map(normal).join("\n")).digest("hex");
    const cat = join(dir, "catalogue.json");
    writeFileSync(cat, JSON.stringify({ operations: [{ case: "stream", op: "session", digest: digest(frames) }, { case: "stream", op: "terminal", digest: digest(term) }] }));
    const { pass, report } = await check(dir, cat);
    assert.ok(pass, JSON.stringify(report));
    assert.equal(report.length, 2);
    writeFileSync(cat, JSON.stringify({ operations: [{ case: "stream", op: "session", digest: "0" }] }));
    assert.equal((await check(dir, cat)).pass, false);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
