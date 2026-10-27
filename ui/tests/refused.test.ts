import assert from "node:assert/strict";
import { test } from "node:test";
import type { RefusedMessages } from "../src/api/types.ts";
import { newestFirst, refusedFlag, refusedLately, refusedLine } from "../src/views/refused.ts";

const NOW = 1_800_000_000_000;
const DAY = 86_400_000;
const fedora = { runner: { name: "fedora", instance: "main" } };
const adacachy = { runner: { name: "adacachy", instance: "main" } };

const none: RefusedMessages = { counts: [], newest: [] };
const one: RefusedMessages = {
  counts: [{ wire: "runner", code: "unknown", n: 1 }],
  newest: [{ at: NOW - 2 * 60_000, unread: { wire: "runner", from: fedora, what: "variant 51", code: "unknown", why: "AgentMsg has 51 variants" } }],
};
// What the server couldn't read of the runner, what the runner couldn't read of the server's, and reach's.
const many: RefusedMessages = {
  counts: [
    { wire: "server_item", code: "malformed", n: 3 },
    { wire: "runner", code: "unknown", n: 9 },
    { wire: "reach_link", code: "malformed", n: 3 },
  ],
  newest: [
    { at: NOW - 3 * DAY, unread: { wire: "server_item", from: fedora, what: "item 4", code: "malformed", why: "bytes left over" } },
    { at: NOW - 40 * 60_000, unread: { wire: "runner", from: fedora, what: "variant 51", code: "unknown", why: "AgentMsg has 51 variants" } },
    { at: NOW - 12_000, unread: { wire: "reach_link", from: adacachy, what: "Say variant 9", code: "malformed", why: "cut short" } },
  ],
};

test("a runner's Refused messages row: none, or how many by wire and code, the most first, and the newest with when, what and why", () => {
  assert.equal(refusedLine(undefined, NOW), "none");
  assert.equal(refusedLine(none, NOW), "none");
  assert.equal(refusedLine(one, NOW), "1 (runner unknown) · 2m ago, variant 51: AgentMsg has 51 variants");
  assert.equal(refusedLine(many, NOW), "15 (runner unknown 9, server_item malformed 3, reach_link malformed 3) · newest 12s ago, Say variant 9: cut short");
  // The server's clock ahead of the page's: it came now, not in a while.
  assert.equal(refusedLine({ ...one, newest: one.newest.map((m) => ({ ...m, at: NOW + 30_000 })) }, NOW), "1 (runner unknown) · now, variant 51: AgentMsg has 51 variants");
});

test("the row is in the fail tone while any came within a day", () => {
  assert.equal(refusedLately(undefined, NOW), false);
  assert.equal(refusedLately(none, NOW), false);
  assert.equal(refusedLately(one, NOW), true);
  assert.equal(refusedLately(one, NOW + DAY), false);
  assert.equal(refusedLately(many, NOW), true);
  assert.equal(refusedLately(many, NOW + DAY), false);
});

test("an agent's flag: none while none was refused, then how many; it opens the list, the newest first", () => {
  assert.equal(refusedFlag(undefined), null);
  assert.equal(refusedFlag(none), null);
  assert.equal(refusedFlag(one), "1 message refused");
  assert.equal(refusedFlag(many), "15 messages refused");
  assert.deepEqual(
    newestFirst(many).map((m) => m.unread.what),
    ["Say variant 9", "variant 51", "item 4"],
  );
  assert.deepEqual(newestFirst(undefined), []);
});
