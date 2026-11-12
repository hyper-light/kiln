import assert from "node:assert/strict";
import { test } from "node:test";
import { syntaxOf, tokensOf } from "../src/views/ship/review/syntax.ts";
import type { Tok } from "../src/lib/syntax/engine.ts";

test("W26-R07, R15: worker texts preserve multiline context and map old/new hunk sides to their own line numbers", () => {
  const syntax = syntaxOf([
    { old_start: 97, old_lines: 3, new_start: 101, new_lines: 3, lines: [" /* context", "-old value", "+new value", " */", "\\ No newline at end of file"] },
    { old_start: 200, old_lines: 0, new_start: 204, new_lines: 1, lines: ["+added"] },
    { old_start: 300, old_lines: 1, new_start: 305, new_lines: 0, lines: ["-deleted"] },
  ]);
  assert.deepEqual(syntax.texts, ["/* context\nold value\n*/", "/* context\nnew value\n*/", "added", "deleted"]);
  const colour = (name: string): Tok[] => [[0, 2, name]];
  const result = tokensOf(syntax, [
    [colour("old-context"), colour("deleted"), colour("old-end")],
    [colour("new-context"), colour("added"), colour("new-end")],
    [colour("only-new")],
    [colour("only-old")],
  ]);
  assert.deepEqual([...result.old.keys()], [97, 98, 99, 300]);
  assert.deepEqual([...result.neu.keys()], [101, 102, 103, 204]);
  assert.deepEqual(result.old.get(98), colour("deleted"));
  assert.deepEqual(result.neu.get(102), colour("added"));
  assert.deepEqual(result.neu.get(204), colour("only-new"));
  assert.equal(result.old.has(204), false);
  assert.equal(result.neu.has(300), false);
});

test("W26-R15: pending or failed syntax leaves the complete diff plain", () => {
  const syntax = syntaxOf([{ old_start: 1, old_lines: 1, new_start: 1, new_lines: 1, lines: ["-before", "+after"] }]);
  assert.equal(tokensOf(syntax, null).old.size, 0);
  assert.equal(tokensOf(syntax, [null, null]).neu.size, 0);
  assert.deepEqual(syntax.texts, ["before", "after"]);
});
