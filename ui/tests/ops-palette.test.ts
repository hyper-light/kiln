/** The palette's entries for kiln's operations read each one's summary, without its asides, to its first clause (api/ops.ts opLabel). */
import assert from "node:assert/strict";
import { test } from "node:test";
import { opLabel } from "../src/api/ops";

test("an operation's entry is its summary without asides, to its first clause", () => {
  assert.equal(opLabel("Publish a port into a running microVM session, in place (no rebuild, no new microVM): the host's address"), "Publish a port into a running microVM session");
  assert.equal(opLabel("Take a folder (`path`) back from a running session: out of its harness's allowed folders"), "Take a folder back from a running session");
  assert.equal(opLabel("Agent definitions (an agent as data: one of kiln's ten harnesses, its model): yours and kiln's"), "Agent definitions");
  assert.equal(opLabel("Delete one of your agent definitions and its versions."), "Delete one of your agent definitions and its versions");
  assert.equal(
    opLabel("Grant a running microVM session's `member` (`harness`, the default, `agent:<name>` or `mcp:<name>`) egress to `host` (a DNS name or an address) on `port`, in place: nothing else"),
    "Grant a running microVM session's member egress to host on port",
  );
});
