import assert from "node:assert/strict";
import { test } from "node:test";
import type { IncomingMessage, ServerResponse } from "node:http";
import type { HEvent } from "../src/api/harness.ts";
import { defaultState } from "../mock/defaults.ts";
import { handleMockHistory, mockHistory } from "../mock/history.ts";

await test("W21-R02 demo bounded snapshots keep legacy behavior, sparse pending context and a contiguous cursor", () => {
  const events: HEvent[] = Array.from({ length: 100 }, (_, i) => ({ id: `e${i + 1}`, seq: i + 1, at: 1, kind: i % 10 === 0 ? "user" : "assistant", text: "x".repeat(500) }));
  events[1] = { id: "old-context", seq: 2, at: 1, kind: "tool", tool_id: "pending-tool", name: "Bash", tool: "shell", title: "Pending shell", input: {}, status: "running", started: 1 };
  const state = defaultState();
  const legacy = mockHistory("bounded-test", () => events, state);
  assert.equal(legacy.events, events);
  assert.equal(legacy.frame, null);
  const bounded = mockHistory("bounded-test", () => events, state, new URLSearchParams("recent_turns=1&event_bytes=16384"));
  assert.equal(bounded.frame?.before, 91);
  assert.equal(bounded.frame?.has_more, true);
  assert.ok(bounded.events.some((e) => e.id === "old-context"));
  assert.ok(!bounded.events.some((e) => e.id === "e50"));
  assert.ok(bounded.events.length < 20);
  let body = "";
  const res = {
    setHeader() {},
    writeHead() {},
    end(text: string) {
      body = text;
    },
  } as unknown as ServerResponse;
  const req = { method: "GET" } as IncomingMessage;
  const url = new URL(`http://mock/api/harness/sessions/bounded-test/events?before=91&cursor=${bounded.frame?.cursor}&limit=10&bytes=16384`);
  assert.equal(handleMockHistory(req, res, url), true);
  const page = JSON.parse(body) as { events: HEvent[]; history: { before: number; has_more: boolean } };
  assert.equal(page.events.length, 10);
  assert.equal(page.history.before, 81);
  assert.equal(page.history.has_more, true);
  assert.equal(page.events[0]?.id, "e81");
  assert.equal(page.events.at(-1)?.id, "e90");
});

await test("W21-R02 demo whole-event budget exceptions preserve text and still advance the cursor", () => {
  const huge = "x".repeat(100000);
  const events: HEvent[] = [
    { id: "u", seq: 1, at: 1, kind: "user", text: "Question" },
    { id: "a", seq: 2, at: 1, kind: "assistant", text: huge },
  ];
  const bounded = mockHistory("huge-test", () => events, defaultState(), new URLSearchParams("recent_turns=1&event_bytes=16384"));
  assert.equal(bounded.frame?.oversized, true);
  assert.equal(bounded.frame?.before, 2);
  assert.equal(bounded.events.at(-1)?.kind === "assistant" && bounded.events.at(-1)?.text, huge);
});
