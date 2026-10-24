// W21-AC01 for the pages no other check drives (docs/ci.md), against insights_setup.py's server, in
// Chromium and WebKit at 1440×900 and 390×844: each page's primary task end to end, then nothing on
// it scrolling sideways and nothing fixed but the shell's own (layout.mjs), and a screenshot.
// - A leg's page: its step's log opened and searched for what the step printed.
// - A run's tests (W16-R03): each with its last results on the branch, a bar each.
// - A run's data flow (W04-R23): an output from its step to the job that reads it and where, and
//   an exported environment and the job that takes it.
// - Attaching an agent where it watches (W20-R08): a repository's Watched by, and its row of three
//   to attach one (its name, its agent, what wakes it).
// - Comparing two runs: what differs, the files changed between them, the steps side by side.
// - Log search: what every run printed, found among the repository's runs.
// - The land queue, and a diff's stack.
// - Add machines: the one command that adds a machine.
//
//     docker run --rm --network host -e INSIGHTS="$(cat /tmp/insights.json)" \
//       -v $PWD/ci/ui:/check:ro -v <dir with node_modules/playwright-core>:/pw:ro -v /tmp/pages-shots:/shots \
//       mcr.microsoft.com/playwright:v1.64.1-noble node /check/pages_check.mjs
import { chromium, webkit, devices } from "/pw/node_modules/playwright-core/index.mjs";
import { checkLayout, pageErrors } from "./layout.mjs";

const cfg = JSON.parse(process.env.INSIGHTS);
const shots = "/shots";
const report = [];
let failed = 0;
const check = (b, what, ok, detail = "") => {
  report.push(`${ok ? "ok  " : "FAIL"} ${b}: ${what}${detail ? ` (${detail})` : ""}`);
  if (!ok) failed++;
};
async function until(f, ms = 15000) {
  const end = Date.now() + ms;
  while (Date.now() < end) {
    if (await f().catch(() => false)) return true;
    await new Promise((r) => setTimeout(r, 200));
  }
  return false;
}
const text = async (loc) => (await loc.innerText().catch(() => "")).replace(/\s+/g, " ");
const flow = async (label, f) => {
  try {
    await f();
  } catch (e) {
    check(label, "ran to its end", false, String(e).split("\n")[0]);
  }
};

const run = cfg.runs[cfg.runs.length - 1];
const base = cfg.runs[0];
const NOTE = "a note from the repository's own action";
const diff = await (await fetch(`${cfg.base}/api/diffs/D1`, { headers: { Authorization: `Bearer ${cfg.admin}` } })).json();

for (const [name, type, phone] of [
  ["chromium", chromium, { ...devices["Pixel 7"], viewport: { width: 390, height: 844 } }],
  ["webkit", webkit, devices["iPhone 13"]],
]) {
  const browser = await type.launch();
  try {
    for (const [tag, options] of [
      ["wide", { viewport: { width: 1440, height: 900 } }],
      ["phone", phone],
    ]) {
      const who = `${name}: ${tag}`;
      const ctx = await browser.newContext(options);
      const r = await ctx.request.post(`${cfg.base}/api/users/lead/link`, { headers: { Authorization: `Bearer ${cfg.admin}` } });
      const signIn = await ctx.newPage();
      await signIn.goto((await r.json()).url);
      await signIn.close();
      const page = await ctx.newPage();
      const errors = pageErrors(page, cfg.base);
      const shot = (what) => page.screenshot({ path: `${shots}/${what}-${name}-${tag}.png`, fullPage: true });

      await flow(`${who}: a leg's page`, async () => {
        await page.goto(`${cfg.base}/runs/${run}`);
        await page.getByRole("navigation", { name: "Jobs" }).getByRole("button", { name: /bench/ }).first().click();
        await page.waitForURL(/\/jobs\//, { timeout: 15000 });
        await page.getByRole("button", { name: /note/ }).first().click();
        await page.getByRole("button", { name: "Search log" }).first().click();
        await page.getByLabel("Find in log").first().fill(NOTE);
        await page.getByLabel("Find in log").first().press("Enter");
        // Enter goes to the next match: whichever it is, of at least one.
        const found = page.locator("text=/^\\d+\\/[1-9]\\d*$/").first();
        check(who, "a leg's page: its step's log, searched", await until(async () => (await found.count()) > 0, 20000), await text(page.locator("main").first()));
        await checkLayout(check, `${who}: a leg's page`, page);
        await shot("leg");
      });

      await flow(`${who}: a run's tests`, async () => {
        await page.goto(`${cfg.base}/runs/${run}`);
        const spark = page.getByRole("img", { name: /^Its last \d+ results: \d+ passed/ }).first();
        check(
          who,
          "a run's tests: each with its last results, a bar each",
          await until(async () => (await spark.count()) > 0 && (await spark.locator("i").count()) > 1, 20000),
          await spark.getAttribute("aria-label").catch(() => ""),
        );
        await checkLayout(check, `${who}: a run's tests`, page);
      });

      await flow(`${who}: a run's data flow`, async () => {
        await page.goto(`${cfg.base}/runs/${cfg.flow_run}`);
        const panel = page.getByRole("list", { name: "The run's outputs and exports" });
        check(
          who,
          "a run's data flow: the output, its step and value, the job that reads it and where",
          await until(async () => {
            const t = await text(panel);
            return /make\.outputs\.version/.test(t) && /from its step v/.test(t) && /4\.2\.0/.test(t) && /use/.test(t) && /env\.VERSION/.test(t);
          }, 20000),
          await text(panel),
        );
        check(who, "and the exported environment, the job that takes it", /make\.exports\.env/.test(await text(panel)) && /TOOL_HOME/.test(await text(panel)) && /env-from/.test(await text(panel)), await text(panel));
        await checkLayout(check, `${who}: a run's data flow`, page);
        await shot("flow");
      });

      await flow(`${who}: attaching an agent where it watches`, async () => {
        await page.goto(`${cfg.base}/repos`);
        await page.locator("tr", { hasText: "acme/app" }).first().locator("td").first().click();
        const watch = page.getByRole("region", { name: "Watched by" }).first();
        check(who, "a repository's Watched by, in its panel", await until(async () => (await watch.count()) > 0, 20000));
        await watch.getByRole("button", { name: /Attach/ }).click();
        const form = watch.locator("form.watch-form");
        check(
          who,
          "attaching one: its name, its agent and what wakes it, a run failing first",
          await until(async () => /a run fails/.test(await text(form)) && (await form.getByLabel("Name").count()) > 0 && (await form.getByLabel("Agent").count()) > 0, 20000),
          await text(form),
        );
        await checkLayout(check, `${who}: attaching an agent`, page);
        await shot("attach");
        await form.getByRole("button", { name: "Cancel" }).click();
        check(who, "and let go of", await until(async () => (await form.count()) === 0, 10000));
      });

      await flow(`${who}: comparing runs`, async () => {
        await page.goto(`${cfg.base}/runs/${run}/compare?with=${base}`);
        const main = page.locator("main").first();
        check(who, "comparing runs: what differs, the files, the steps", await until(async () => /What differs/i.test(await text(main)) && /round\.txt/.test(await text(main)) && /Steps|Legs/i.test(await text(main)), 20000), (await text(main)).slice(0, 300));
        await checkLayout(check, `${who}: comparing runs`, page);
        await shot("compare");
      });

      await flow(`${who}: log search`, async () => {
        await page.goto(`${cfg.base}/logs`);
        await page.getByLabel("Text").fill(NOTE);
        await page.getByLabel("Text").press("Enter");
        const main = page.locator("main").first();
        check(who, "log search: what every run printed, found", await until(async () => /acme\/app/.test(await text(main)) && (await text(main)).includes(NOTE), 20000), (await text(main)).slice(0, 300));
        await checkLayout(check, `${who}: log search`, page);
        await shot("logs");
      });

      await flow(`${who}: the land queue`, async () => {
        await page.goto(`${cfg.base}/review/queue`);
        check(who, "the land queue", await until(async () => /Land queue/i.test(await text(page.locator("main").first())), 20000));
        await checkLayout(check, `${who}: the land queue`, page);
        await shot("queue");
      });

      await flow(`${who}: a diff's stack`, async () => {
        await page.goto(`${cfg.base}/review/stacks/${encodeURIComponent(diff.stack)}`);
        check(who, "a diff's stack, its diff in it", await until(async () => /D1/.test(await text(page.locator("main").first())), 20000), (await text(page.locator("main").first())).slice(0, 200));
        await checkLayout(check, `${who}: a diff's stack`, page);
        await shot("stack");
      });

      await flow(`${who}: Add machines`, async () => {
        await page.goto(`${cfg.base}/runners/add`);
        const main = page.locator("main").first();
        check(who, "Add machines: the command that adds one", await until(async () => /Add a machine/i.test(await text(main)) && /install\.(sh|ps1)|kiln/.test(await text(main.locator("code, pre").first())), 20000), (await text(main)).slice(0, 300));
        await checkLayout(check, `${who}: Add machines`, page);
        await shot("add-machines");
      });

      check(who, "no page errors", errors.length === 0, errors.join("; "));
      await ctx.close();
    }
  } finally {
    await browser.close();
  }
}

console.log(report.join("\n"));
process.exit(failed ? 1 : 0);
