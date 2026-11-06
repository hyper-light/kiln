/**
 * bp-g2 steps 4 and 8: kiln-bp's shared vectors in the browsers themselves (Chromium and WebKit):
 * the codec bundled into a blank page reads every vector and join as Rust did, a block of the
 * seeded fuzz inputs to Rust's digest, and one long-lived DecompressionStream("deflate-raw") gives
 * each sync-flushed batch back whole before the next. Run with `npx playwright test -c
 * playwright.bp.config.ts` (no server needed).
 */
import { readFileSync } from "node:fs";
import { build } from "esbuild";
import { expect, test } from "@playwright/test";

test("the shared vectors read in this browser as in Rust", async ({ page }) => {
  const bundle = await build({ entryPoints: [new URL("../tests/bp-check.ts", import.meta.url).pathname], bundle: true, write: false, format: "iife", globalName: "bp", target: "es2022" });
  const code = bundle.outputFiles[0]!.text;
  const vectors = JSON.parse(readFileSync(new URL("../tests/bp-vectors.json", import.meta.url), "utf8"));
  await page.goto("about:blank");
  await page.addScriptTag({ content: code });
  const r = await page.evaluate(async ([v]) => (globalThis as unknown as { bp: { check: (v: unknown, n: number) => Promise<{ checked: number; failed: string[] }> } }).bp.check(v, 2), [vectors]);
  expect(r.failed).toEqual([]);
  expect(r.checked).toBeGreaterThan(80);
});
