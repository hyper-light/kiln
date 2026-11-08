import assert from "node:assert/strict";
import { registerHooks } from "node:module";
import { test } from "node:test";
import * as React from "react";
import { renderToStaticMarkup } from "react-dom/server";
import type { ThreadComment } from "../src/api/review.ts";

// This tab's storage, as a browser has it (review-composer.test.ts): the thread's modules read
// drafts from it.
const values = new Map<string, string>();
const storage = {
  getItem: (key: string) => values.get(key) ?? null,
  setItem: (key: string, value: string) => void values.set(key, value),
  removeItem: (key: string) => void values.delete(key),
};
Object.assign(globalThis, { window: new EventTarget(), document: new EventTarget(), localStorage: storage, sessionStorage: storage });

// The real thread's line; its stylesheets need no CSS in Node.
const loader = registerHooks({
  load(url, context, nextLoad) {
    return url.endsWith(".css") ? { format: "module", source: "export {};", shortCircuit: true } : nextLoad(url, context);
  },
});
const { bylineOf, sealOf } = await import("../src/api/review.ts");
const { shortIds } = await import("../src/lib/format.ts");
const { CommentWho } = await import("../src/views/ship/review/threads.tsx");
loader.deregister();

type Who = Pick<ThreadComment, "author" | "by" | "signed">;
const line = (c: Who) => renderToStaticMarkup(React.createElement(CommentWho, { c }));
const has = (html: string, ...parts: string[]) => parts.forEach((p) => assert.ok(html.includes(p), `expected ${JSON.stringify(p)} in\n${html}`));
const lacks = (html: string, ...parts: string[]) => parts.forEach((p) => assert.ok(!html.includes(p), `did not expect ${JSON.stringify(p)} in\n${html}`));

const KIM: Who = { author: "user:kim", by: { kind: "person", name: "kim" }, signed: null };
const MATRIX: Who = {
  author: "agent:01a19c0ffee0",
  by: { kind: "agent", name: "Matrix agent", for: "kim" },
  signed: { by: "kiln://acme/person/kim/agent/01a19c0ffee0/epoch/1", who: "Matrix agent for kim", at: 1793500000 },
};

test("w26_r18 a comment's line names its author in words: a person by name, an agent for its person", () => {
  assert.equal(bylineOf(KIM), "kim");
  assert.equal(bylineOf(MATRIX), "Matrix agent for kim");
  // A server from before sends the principal alone.
  assert.equal(bylineOf({ author: "agent:01a19c0f" }), "agent 01a19c0f");
  // A session kiln names whole reads by its id's first eight, as it always did (the DB query audit, 2026-11-06).
  assert.equal(bylineOf({ author: "agent:01a1b00011112222333344445555aaaa@m1" }), "agent 01a1b000@m1");
  assert.equal(shortIds("agent:01a1b00011112222333344445555aaaa@m1 for user:ada"), "agent:01a1b000@m1 for user:ada");
  assert.equal(shortIds("shell:01a1b00011112222333344445555aaaa and agent:review"), "shell:01a1b000 and agent:review");
  assert.equal(shortIds("agent:01a1b00011112222333344445555aaaaff"), "agent:01a1b00011112222333344445555aaaaff", "not a session's id: more digits");
  const kim = line(KIM);
  has(kim, "kim");
  lacks(kim, "rv-for");
  const matrix = line(MATRIX);
  has(matrix, "Matrix agent", '<span class="rv-for">for kim</span>');
});

test("w26_r18 the seal says who signed a comment, or that it isn't signed, and is whole only when signed", () => {
  assert.equal(sealOf(MATRIX), "signed by Matrix agent for kim");
  assert.equal(sealOf(KIM), "unsigned");
  assert.equal(sealOf({}), "unsigned", "a server from before says nothing of signatures");
  has(line(MATRIX), 'class="rv-seal"', 'data-signed=""', 'title="signed by Matrix agent for kim"', 'aria-label="signed by Matrix agent for kim"');
  const kim = line(KIM);
  has(kim, 'title="unsigned"', 'aria-label="unsigned"');
  lacks(kim, "data-signed");
});

test("w26_r18 a person's line and an agent's are one shape: the mark and name, then the seal", () => {
  const spans = (html: string) => [...html.matchAll(/class="(rv-[a-z]+)"/g)].map((m) => m[1]);
  assert.deepEqual(spans(line(KIM)), ["rv-by", "rv-seal"]);
  assert.deepEqual(spans(line(MATRIX)), ["rv-by", "rv-for", "rv-seal"], "the agent's person its one extra");
});
