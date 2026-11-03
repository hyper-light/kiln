import assert from "node:assert/strict";
import { registerHooks } from "node:module";
import { test } from "node:test";
import * as React from "react";
import { renderToStaticMarkup } from "react-dom/server";

// This tab's storage, as a browser has it (review-byline.test.ts): the thread's modules read drafts from it.
const values = new Map<string, string>();
const storage = {
  getItem: (key: string) => values.get(key) ?? null,
  setItem: (key: string, value: string) => void values.set(key, value),
  removeItem: (key: string) => void values.delete(key),
};
Object.assign(globalThis, { window: new EventTarget(), document: new EventTarget(), localStorage: storage, sessionStorage: storage });

// The real thread's modules; their stylesheets need no CSS in Node.
const loader = registerHooks({
  load(url, context, nextLoad) {
    return url.endsWith(".css") ? { format: "module", source: "export {};", shortCircuit: true } : nextLoad(url, context);
  },
});
const { ThreadFix } = await import("../src/views/ship/review/threads.tsx");
loader.deregister();

const fix = (t: { fixed_in?: number | null; waits?: string | null }) => renderToStaticMarkup(React.createElement(ThreadFix, { t }));

test("w26_r18 an open thread says the version that fixed it, as a suggestion says it was applied", () => {
  const html = fix({ fixed_in: 6 });
  assert.ok(html.includes("fixed in v6") && html.includes("md-applied"), html);
  assert.equal(fix({}), "", "nothing until a version answers it");
  assert.equal(fix({ fixed_in: null, waits: null }), "");
});

test("w26_ac10 a thread agents have fixed three times since a person spoke waits for a person, saying why", () => {
  const why = "agents have started 3 fixes in this thread since a person spoke in it: it waits for a person";
  const html = fix({ fixed_in: 4, waits: why });
  assert.ok(html.includes("waits for a person") && html.includes(`title="${why}"`) && html.includes('data-waits="1"'), html);
  assert.ok(!html.includes("fixed in"), "waiting says more than the last fix");
});
