#!/usr/bin/env node
/**
 * Goal 8's pages loaded from `base` in each engine at desktop and phone widths: fails on a page
 * error, an asset that didn't load, or a page still on its splash once the network is quiet
 * (goal 8, plan 01a1a832ebfd: an import map, packed files and deferred parts must leave
 * every page working). Writes a still of each to SHOTS (ci/ui/shots/g8) when set.
 *
 *   node ci/ui/smoke.mjs <base> [chromium] [webkit]
 */
import { createRequire } from "node:module";
import { mkdirSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const pw = createRequire(path.join(HERE, "..", "..", "ui", "package.json"))("@playwright/test");
const [base, ...engines] = process.argv.slice(2);
const SHOTS = process.env.SHOTS;

async function firstRun() {
  try {
    const res = await fetch(`${base}/api/runs?limit=1`);
    const runs = await res.json();
    return (Array.isArray(runs) ? runs : (runs.runs ?? []))[0]?.id ?? null;
  } catch {
    return null;
  }
}

const run = await firstRun();
const PAGES = [["overview", "/"], ["mission", "/agents"], ["conversation", "/agents/demo-long"], ["runs", "/runs"], ...(run ? [["run", `/runs/${run}`]] : [])];
let failed = 0;
for (const engine of engines.length ? engines : ["chromium", "webkit"]) {
  const type = engine === "webkit" ? pw.webkit : pw.chromium;
  const browser = await type.launch(engine === "chromium" && process.env.E2E_CHANNEL ? { channel: process.env.E2E_CHANNEL } : {});
  for (const form of ["desktop", "phone"]) {
    const device = form === "phone" ? (engine === "webkit" ? pw.devices["iPhone 15"] : pw.devices["Pixel 7"]) : { viewport: { width: 1440, height: 900 } };
    for (const [name, at] of PAGES) {
      const ctx = await browser.newContext(device);
      const page = await ctx.newPage();
      const problems = [];
      page.on("pageerror", (e) => problems.push(`page error: ${e.message}`));
      page.on("console", (m) => m.type() === "error" && /import map|module specifier|Failed to fetch dynamically|preload|Unable to/i.test(m.text()) && problems.push(`console: ${m.text()}`));
      page.on("response", (r) => r.url().includes("/assets/") && r.status() >= 400 && problems.push(`${r.status()} ${r.url()}`));
      page.on("requestfailed", (r) => r.url().includes("/assets/") && problems.push(`failed ${r.url()}: ${r.failure()?.errorText}`));
      try {
        await page.goto(base + at, { waitUntil: "load", timeout: 120_000 });
        await page.waitForTimeout(4000);
        if (await page.locator('[aria-busy="true"] >> text=Loading kiln').count()) problems.push("still on the splash");
      } catch (e) {
        problems.push(`load: ${e.message.split("\n")[0]}`);
      }
      if (SHOTS) {
        mkdirSync(SHOTS, { recursive: true });
        await page.screenshot({ path: path.join(SHOTS, `${name}-${engine}-${form}.png`) }).catch(() => {});
      }
      console.log(`${problems.length ? "FAIL" : "ok  "} ${engine.padEnd(8)} ${form.padEnd(7)} ${name}${problems.length ? `\n      ${problems.slice(0, 5).join("\n      ")}` : ""}`);
      failed += problems.length ? 1 : 0;
      await ctx.close();
    }
  }
  await browser.close();
}
process.exitCode = failed ? 1 : 0;
