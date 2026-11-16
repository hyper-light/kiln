import assert from "node:assert/strict";
import { test } from "node:test";
import { asksOnly } from "../src/views/terminal/replies.ts";

test("W21-R02 a colour sequence that only asks is nothing but its answers; one that sets a colour isn't", () => {
  // The text's, the background's and the cursor's colour: one value each, in order.
  assert.equal(asksOnly(11, "?"), true);
  assert.equal(asksOnly(10, "?;?;?"), true);
  assert.equal(asksOnly(11, "#102030"), false);
  assert.equal(asksOnly(10, "red;?"), false, "it sets the first");
  assert.equal(asksOnly(12, ""), false, "nothing asked");
  // A palette's: a colour's number, then its value.
  assert.equal(asksOnly(4, "1;?"), true);
  assert.equal(asksOnly(4, "1;?;2;?"), true);
  assert.equal(asksOnly(4, "1;rgb:ff/00/00"), false);
  assert.equal(asksOnly(4, "1;?;2;rgb:ff/00/00"), false);
  assert.equal(asksOnly(4, "1"), false, "a number without its value");
});
