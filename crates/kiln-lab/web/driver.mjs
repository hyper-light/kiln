// kiln's browser lab driver (docs/browser-lab.md; goal 1 of the browser protocol program, plan
// 01a1a832e9cc). kiln-lab runs it inside a client's network namespace, one browser each:
//   node driver.mjs probe          does each browser start here (one JSON line each)
//   node driver.mjs job JOB.json   run a job's operations, writing its samples beside it
import { chromium, firefox, webkit } from "playwright";

const BROWSERS = { chromium, webkit, firefox };

async function probe() {
  for (const [name, type] of Object.entries(BROWSERS)) {
    try {
      const browser = await type.launch();
      const page = await browser.newPage();
      await page.setContent("<p>lab</p>");
      console.log(JSON.stringify({ browser: name, ok: true, version: browser.version() }));
      await browser.close();
    } catch (e) {
      console.log(JSON.stringify({ browser: name, ok: false, error: String(e && e.message ? e.message : e) }));
    }
  }
}

const [mode, arg] = process.argv.slice(2);
if (mode === "probe") {
  await probe();
} else if (mode === "job") {
  const { runJob } = await import("./job.mjs");
  await runJob(arg);
} else {
  console.error("usage: node driver.mjs probe | job JOB.json");
  process.exitCode = 2;
}
