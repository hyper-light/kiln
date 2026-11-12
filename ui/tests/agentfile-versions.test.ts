// An Agentfile's versions as its page lists them (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 31):
// newest first, the current one said, every other one to read and make current again, and none of
// those while the editor holds unsaved changes.
import assert from "node:assert/strict";
import { registerHooks } from "node:module";
import { test } from "node:test";
import * as React from "react";
import { renderToStaticMarkup } from "react-dom/server";
import type { Agentfile, AgentfileVersion } from "../src/api/wire/index.ts";

Object.assign(globalThis, {
  window: Object.assign(new EventTarget(), { setTimeout, clearTimeout, setInterval, clearInterval }),
  document: new EventTarget(),
});
// The real component; its stylesheets need no CSS in Node.
const loader = registerHooks({
  load(url, context, nextLoad) {
    return url.endsWith(".css") ? { format: "module", source: "export {};", shortCircuit: true } : nextLoad(url, context);
  },
});
const { seedResource } = await import("../src/lib/resource.ts");
const { agentfileVersionsRes } = await import("../src/api/agentfiles.ts");
const { Versions } = await import("../src/views/agentfiles/agentfile.tsx");
loader.deregister();

const now = Math.floor(Date.now() / 1000);
const kept: Agentfile = { id: "01a1cccccccccccccccccccccccccccc", owner: "owner", name: "claude-rust", text: "FROM rust\n", version: 3, created_at: now - 300, updated_at: now - 60 };
const versions: AgentfileVersion[] = [3, 2, 1].map((v) => ({ version: v, text: `FROM rust:1.${v}\n`, at: now - 100 * v, by: v === 2 ? "user:kim" : "admin" }));
const res = agentfileVersionsRes(kept.id);
assert.equal(seedResource(res.key, versions, res.fetch, res.opts), true);

const list = (dirty: boolean) => renderToStaticMarkup(React.createElement(Versions, { a: kept, dirty, onRestored: () => undefined, onFailed: () => undefined }));
const rows = (html: string) => html.split("<li>").slice(1);

test("step31: its versions newest first, the current one said, each other one to read and make current", () => {
  const html = list(false);
  const [v3, v2, v1] = rows(html);
  assert.equal(rows(html).length, 3);
  assert.match(v3 ?? "", /v3<\/span>.*· admin · current/);
  assert.ok(!(v3 ?? "").includes("Make current"), "the current one isn't made current again");
  for (const r of [v2, v1]) {
    assert.ok((r ?? "").includes(">Read</button>"), r);
    assert.match(r ?? "", /title="Its text current again, as v4"[^>]*>Make current<\/button>/);
  }
  assert.match(v2 ?? "", /· user:kim</);
});

test("step31: while the editor holds unsaved changes, no version is made current over them", () => {
  for (const r of rows(list(true)).slice(1)) assert.match(r, /disabled=""[^>]*title="Save or discard your changes first"[^>]*>Make current<\/button>/);
});
