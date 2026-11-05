/**
 * bp-g2 step 10: the browser's decode cost per frame, protocol against JSON.parse of today's
 * frame for the same operation, in this engine (Chromium and WebKit; `-c playwright.bp.config.ts`).
 * Prints a row per sequence, µs per frame at p50, p99 and p99.9.
 */
import { readFileSync } from "node:fs";
import { build } from "esbuild";
import { expect, test } from "@playwright/test";

test("decode cost per frame, protocol against JSON.parse", async ({ page, browserName }, info) => {
  const bundle = await build({ entryPoints: [new URL("../tests/bp-bench.ts", import.meta.url).pathname], bundle: true, write: false, format: "iife", globalName: "bpb", target: "es2022" });
  const m = JSON.parse(readFileSync(new URL("../tests/bp-measure.json", import.meta.url), "utf8")) as { frames: unknown[] };
  await page.goto("about:blank");
  await page.addScriptTag({ content: bundle.outputFiles[0]!.text });
  const rows = await page.evaluate(([f]) => (globalThis as unknown as { bpb: { bench: (f: unknown) => { seq: string; frames: number; bp: number[]; json: number[] }[] } }).bpb.bench(f), [m.frames]);
  for (const r of rows)
    console.log(
      `decode ${info.project.name} ${browserName} | ${r.seq} | ${r.frames} | bp ${r.bp.map((x) => x.toFixed(2)).join(" / ")} µs | JSON.parse ${r.json.map((x) => x.toFixed(2)).join(" / ")} µs`,
    );
  expect(rows.length).toBeGreaterThan(2);
});
