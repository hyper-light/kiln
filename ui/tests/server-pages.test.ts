/**
 * The pages kiln forwards to the UI server (goal 8, plan 01a1a832ebfd step 3): every route outside
 * the browser-only `_app` layout, by the path it starts with; every other page is kiln's own shell.
 */
import assert from "node:assert/strict";
import { readdirSync } from "node:fs";
import { test } from "node:test";
import { prefixOf, serverPages } from "../scripts/server-pages.mjs";

test("a route's prefix is its path up to its first parameter", () => {
  assert.equal(prefixOf("shared.plans.$token.tsx"), "/shared/plans/");
  assert.equal(prefixOf("docs.index.tsx"), "/docs/");
  assert.equal(prefixOf("ship_.deploys.tsx"), "/ship/deploys/");
});

test("only routes outside _app render on the server", () => {
  assert.deepEqual(serverPages(["__root.tsx", "_app.tsx", "_app.agents.$id.tsx", "_app.index.tsx", "shared.plans.$token.tsx"]), ["/shared/plans/"]);
  const today = serverPages(readdirSync(new URL("../src/routes/", import.meta.url)));
  assert.ok(today.includes("/shared/plans/"));
  assert.ok(!today.includes("/"), "a route at / would send every page to the UI server");
});
