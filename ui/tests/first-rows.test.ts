/**
 * W21-R02: how many of Mission control's rows a page can show as it opens is derived from the least a row is tall,
 * which is the style's to say: the number here is held to it.
 */
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { firstRows, ROW_LEAST } from "../src/views/harness/first-rows.ts";

test("W21-R02 the least a row is tall is its style's min-height", () => {
  const css = readFileSync(new URL("../src/views/harness/mission.css", import.meta.url), "utf8");
  const rule = /^\.ms-row \{([^}]*)\}/m.exec(css);
  assert.ok(rule, "mission.css has the row's own rule");
  assert.equal(/min-height:\s*(\d+)px/.exec(rule[1]!)?.[1], String(ROW_LEAST));
});

test("W21-R02 a page opens on no more rows than begin within two viewports of the list's top", () => {
  assert.equal(firstRows(84), 2);
  assert.equal(firstRows(900), Math.ceil(1800 / ROW_LEAST));
  assert.equal(firstRows(85), 3, "a row that only begins there counts");
  assert.equal(firstRows(0), 0);
});
