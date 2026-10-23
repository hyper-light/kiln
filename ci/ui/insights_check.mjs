// Against insights_setup.py's server, in Chromium and WebKit, wide (lead, the owner role) and on
// phones (dev, a developer of acme/app):
// - W16 on Insights: a repository's tests by each sort, the flaky ones with why; its benchmarks;
//   a metric's trend on each kind of runner (chart, reading, table); the run page's link to it;
// - W16-R09: the diff's bench-gate verdict beside its checks;
// - W19-R06: Settings → You, a rule of one's own added and removed;
// - W21-R11: Settings → CI for lead (a secret, a variable, the routing tester, the issuer), none
//   for dev; W20-R03: a routing rule no runner meets, warned of as it's saved;
// - W07-R02: the run page's rerun with fresh actions; W21-R08: the Runners page's executors;
// - W21-R03: the runs list narrowed by words, sorted oldest first, its runner and executor facets;
// - W10: dev's home their repository's, without the server's runners; lead's the server's;
// - no page errors, no failed requests, nothing wider than the page.
import { chromium, webkit, devices } from "/pw/node_modules/playwright-core/index.mjs";
import { checkLayout } from "./layout.mjs";

const cfg = JSON.parse(process.env.INSIGHTS);
const shots = "/shots";
const report = [];
let failed = 0;
const check = (browser, what, ok, detail = "") => {
  report.push(`${ok ? "ok  " : "FAIL"} ${browser}: ${what}${detail ? ` (${detail})` : ""}`);
  if (!ok) failed++;
};

async function signedIn(browser, who, options) {
  const ctx = await browser.newContext(options);
  const r = await ctx.request.post(`${cfg.base}/api/users/${who}/link`, { headers: { Authorization: `Bearer ${cfg.admin}` } });
  if (!r.ok()) throw new Error(`link: ${r.status()} ${await r.text()}`);
  const { url } = await r.json();
  const page = await ctx.newPage();
  await page.goto(url);
  await page.close();
  return ctx;
}

/** Waits until `f` holds, for up to `ms`. */
async function until(f, ms = 15000) {
  const end = Date.now() + ms;
  while (Date.now() < end) {
    if (await f().catch(() => false)) return true;
    await new Promise((r) => setTimeout(r, 200));
  }
  return false;
}

const overflow = (page) => page.evaluate(() => document.scrollingElement.scrollWidth - window.innerWidth);
const panel = (page, title) => page.locator("section.panel").filter({ has: page.locator(".panel-title", { hasText: title }) });

for (const [name, type, phone] of [
  // W21-AC01's phone: 390×844 in both (Chromium as a Pixel at that size).
  ["chromium", chromium, { ...devices["Pixel 7"], viewport: { width: 390, height: 844 } }],
  ["webkit", webkit, devices["iPhone 13"]],
]) {
  const browser = await type.launch();
  try {
    for (const [tag, options, who] of [
      ["wide", { viewport: { width: 1440, height: 900 } }, "lead"],
      ["phone", phone, "dev"],
    ]) {
      const ctx = await signedIn(browser, who, options);
      const page = await ctx.newPage();
      const errors = [];
      page.on("pageerror", (e) => errors.push(String(e)));
      const bad = [];
      page.on("response", (r) => {
        // This server has no UI server: its pages answer 503 with the app's shell, as kiln does then.
        if (r.status() >= 400 && !(r.status() === 503 && r.request().resourceType() === "document")) bad.push(`${r.status()} ${r.url().replace(cfg.base, "")}`);
      });
      await page.goto(`${cfg.base}/insights?repo=acme/app`);
      const tests = panel(page, /^Tests$/);
      await tests.locator("tbody tr").first().waitFor({ timeout: 30000 });
      const firstRow = async () => (await tests.locator("tbody tr").first().innerText()).replace(/\s+/g, " ");
      let row = await firstRow();
      check(name, `${tag}: most failing first`, /rounds/.test(row), row);
      check(name, `${tag}: failures with their share`, /8 · 100%/.test(row), row);
      await page.screenshot({ path: `${shots}/insights-${name}-${tag}-repo.png`, fullPage: true });
      await tests.getByRole("button", { name: "Slowest" }).click();
      await until(async () => /retries a slow network/.test(await firstRow()));
      row = await firstRow();
      check(name, `${tag}: slowest first`, /retries a slow network/.test(row), row);
      if (tag === "wide") check(name, `${tag}: times read short`, /7\.50 s/.test(row), row);
      await tests.getByRole("button", { name: "Flaky" }).click();
      await until(async () => /probably flaky/i.test(await tests.innerText()) || /No flaky tests/.test(await tests.innerText()));
      const flakyText = (await tests.innerText()).replace(/\s+/g, " ");
      check(name, `${tag}: the flipping test is probably flaky, with why`, /divides/.test(flakyText) && /probably flaky/i.test(flakyText) && /changed in/.test(flakyText), flakyText.slice(0, 300));
      check(name, `${tag}: the always-failing test isn't flaky`, !/rounds/.test(flakyText), flakyText.slice(0, 300));
      await page.screenshot({ path: `${shots}/insights-${name}-${tag}-flaky.png`, fullPage: true });
      const bench = panel(page, /^Benchmarks$/);
      await bench.waitFor({ timeout: 15000 });
      const names = (await bench.locator("tbody tr").allInnerTexts()).map((t) => t.replace(/\s+/g, " "));
      check(name, `${tag}: three benchmarks listed`, names.length === 3 && names.some((t) => /parse\/large_file/.test(t)), names.join(" | "));
      check(name, `${tag}: the repository page fits`, (await overflow(page)) <= 0, String(await overflow(page)));
      await checkLayout(check, `${name}: ${tag}: the repository page`, page);
      await bench.getByRole("link", { name: "parse/large_file" }).click();
      await page.waitForURL(/metric=parse/, { timeout: 15000 });
      await page.getByRole("heading", { level: 1, name: "parse/large_file" }).waitFor({ timeout: 15000 });
      await until(async () => /Kinds of runner/.test(await page.locator("dl.facts").innerText()), 30000);
      const facts = (await page.locator("dl.facts").innerText()).replace(/\s+/g, " ");
      check(name, `${tag}: the metric's facts`, /Lower/.test(facts) && /Kinds of runner 2/.test(facts) && /Points 16/.test(facts), facts);
      const trends = page.locator("section.panel").filter({ has: page.locator("canvas") });
      await trends.first().locator("canvas").first().waitFor({ timeout: 15000 });
      // Each chart draws once its own code has loaded: the second may come a moment after the first.
      await until(async () => (await trends.count()) >= 2, 15000);
      const n = await trends.count();
      check(name, `${tag}: a chart per kind of runner`, n === 2, String(n));
      const titles = await trends.locator(".panel-title").allInnerTexts();
      check(name, `${tag}: the latest kind first`, titles.length === 2 && !titles[0].includes("Neoverse-N1") && titles[1].includes("Neoverse-N1"), titles.join(" | "));
      const eyebrow = (await trends.first().locator(".panel-eyebrow").innerText()).replace(/\s+/g, " ");
      check(name, `${tag}: nanoseconds read as milliseconds`, /8 points · latest 1\.\d+ ms/i.test(eyebrow), eyebrow);
      // The reading at a point: its date and time, and the value in the chart's unit.
      const plot = trends.nth(1).locator(".u-over");
      await plot.scrollIntoViewIfNeeded();
      const box = await plot.boundingBox();
      if (box) {
        if (tag === "phone") await page.touchscreen.tap(box.x + box.width * 0.6, box.y + box.height / 2);
        else await page.mouse.move(box.x + box.width * 0.6, box.y + box.height / 2);
        await page.waitForTimeout(300);
      }
      const legend = (await trends.nth(1).locator(".u-legend").innerText().catch(() => "")).replace(/\s+/g, " ");
      check(name, `${tag}: the reading has the date`, /(Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec)/.test(legend) && /ms/.test(legend), legend);
      await trends.first().locator("summary", { hasText: "As a table" }).click();
      const trs = trends.first().locator("details tbody tr");
      check(name, `${tag}: the table has every point`, (await trs.count()) === 8, String(await trs.count()));
      const cell = (await trs.first().innerText()).replace(/\s+/g, " ");
      check(name, `${tag}: a row: when, commit, value`, /[0-9a-f]{7}/.test(cell) && /ms/.test(cell), cell);
      check(name, `${tag}: the metric page fits`, (await overflow(page)) <= 0, String(await overflow(page)));
      await checkLayout(check, `${name}: ${tag}: the metric page`, page);
      await page.screenshot({ path: `${shots}/insights-${name}-${tag}-metric.png`, fullPage: true });
      // The run page: a metric opens its trend.
      await page.goto(`${cfg.base}/runs/${encodeURIComponent(cfg.runs[cfg.runs.length - 1])}`);
      const runBench = panel(page, /^Benchmarks$/);
      await runBench.waitFor({ timeout: 30000 });
      const before = await runBench.innerText();
      check(name, `${tag}: the run's benchmarks compare with a baseline`, /baseline/.test(before), before.replace(/\s+/g, " ").slice(0, 200));
      await page.screenshot({ path: `${shots}/insights-${name}-${tag}-run.png`, fullPage: true });
      // The run pinned its actions: a rerun may look them up again (W07-R02). Asked for, not run.
      const fresh = page.getByRole("checkbox", { name: "Fresh actions" });
      check(name, `${tag}: a rerun may take fresh actions`, await until(async () => await fresh.isVisible()));
      let asked = null;
      await page.route("**/api/ci/runs/*/rerun", async (route) => {
        asked = route.request().postDataJSON();
        await route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify({ run: cfg.runs[cfg.runs.length - 1], attempt: 2 }) });
      });
      await fresh.check();
      await page.getByRole("button", { name: "Rerun", exact: true }).click();
      check(name, `${tag}: and the rerun asks for them`, await until(async () => asked?.fresh_actions === true), JSON.stringify(asked));
      await page.unroute("**/api/ci/runs/*/rerun");
      await runBench.getByRole("link", { name: "throughput" }).first().click();
      await page.waitForURL(/metric=throughput/, { timeout: 15000 });
      await page.getByRole("heading", { level: 1, name: "throughput" }).waitFor({ timeout: 15000 });
      const f2 = (await page.locator("dl.facts").innerText()).replace(/\s+/g, " ");
      check(name, `${tag}: the run page's link opens the trend`, /Higher/.test(f2), f2);
      // The diff that made parse slower: its gate's verdict beside its checks (W16-R09).
      await page.goto(`${cfg.base}/review/D1`);
      const gates = page.locator("ul.rv-gates");
      await gates.waitFor({ timeout: 30000 });
      const gateText = (await gates.innerText()).replace(/\s+/g, " ");
      check(name, `${tag}: the diff shows its failing gate`, /error/i.test(gateText) && /parse\/large_file is \d+\.\d% worse than main/.test(gateText), gateText);
      check(name, `${tag}: the diff page fits`, (await overflow(page)) <= 0, String(await overflow(page)));
      await checkLayout(check, `${name}: ${tag}: the diff page`, page);
      await page.screenshot({ path: `${shots}/insights-${name}-${tag}-diff.png`, fullPage: true });
      // Settings → You: a rule of one's own, added and removed (W19-R06).
      await page.goto(`${cfg.base}/settings#you.notifications`);
      await page.getByRole("button", { name: "Add the rule" }).waitFor({ timeout: 90000 });
      await page.getByRole("checkbox", { name: "run.failed" }).check();
      await page.getByRole("button", { name: "Add the rule" }).click();
      const rules = page.getByRole("list", { name: "Your rules" });
      await rules.waitFor({ timeout: 15000 });
      const ruleText = (await rules.innerText()).replace(/\s+/g, " ");
      check(name, `${tag}: a rule of one's own is kept`, /run\.failed/.test(ruleText) && /your inbox/.test(ruleText) && /only about you/.test(ruleText), ruleText);
      check(name, `${tag}: the settings page fits`, (await overflow(page)) <= 0, String(await overflow(page)));
      await checkLayout(check, `${name}: ${tag}: the settings page`, page);
      await page.screenshot({ path: `${shots}/insights-${name}-${tag}-settings.png`, fullPage: true });
      await rules.getByRole("button", { name: "Remove" }).first().click();
      check(name, `${tag}: and removed`, await until(async () => (await page.getByRole("list", { name: "Your rules" }).count()) === 0));
      // Settings → CI: the owner's; a developer of one repository has none of it (W21-R11).
      await page.goto(`${cfg.base}/settings#ci.secrets`);
      if (who === "dev") {
        await page.getByRole("button", { name: "Add the rule" }).waitFor({ timeout: 90000 });
        check(name, `${tag}: dev has no CI settings`, (await page.getByText("Secrets and variables").count()) === 0);
      } else {
        await page.locator("#st-secret-name").waitFor({ timeout: 30000 });
        await page.locator("#st-secret-name").fill("deploy_key");
        await page.locator("#st-secret-value").fill("s3cret-value-77");
        await page.getByRole("button", { name: "Save the secret" }).click();
        const secrets = page.getByRole("list", { name: "Secrets" });
        const saved = await until(async () => (await page.locator("#st-secret-value").inputValue()) === "" && /DEPLOY_KEY/.test(await secrets.innerText()), 30000);
        check(name, `${tag}: a secret is saved, its name shown and its value not`, saved && !/s3cret-value-77/.test(await page.content()), saved ? "the value is in the page" : "not saved");
        await page.locator("#st-var-name").fill("region");
        await page.locator("#st-var-value").fill("eu-west-1");
        await page.getByRole("button", { name: "Save the variable" }).click();
        check(name, `${tag}: a variable is saved with its value`, await until(async () => /REGION[\s\S]*eu-west-1/.test(await page.getByRole("list", { name: "Variables" }).innerText())));
        await page.locator("#st-route-repo").scrollIntoViewIfNeeded();
        await page.getByLabel("Labels (runs-on)").fill("local");
        await page.getByRole("button", { name: "Explain" }).click();
        const runners = page.getByRole("list", { name: "Runners" });
        check(name, `${tag}: the tester finds the local runner eligible`, await until(async () => /eligible/.test(await runners.innerText()) && !/not eligible/.test(await runners.innerText())), await runners.innerText().catch(() => ""));
        // A rule no runner meets, saved: the server says so (W20-R03); then taken out again.
        const serverRules = page.locator("[id='ci.routing.server']");
        await serverRules.getByRole("button", { name: "Add a rule" }).click();
        const newRule = serverRules.getByRole("group").last();
        await newRule.getByLabel("Labels", { exact: true }).fill("plan9");
        await newRule.getByLabel("Requirements", { exact: true }).fill("os == plan9");
        await serverRules.getByRole("button", { name: "Save the rules" }).click();
        const rulesText = async () => (await serverRules.innerText()).replace(/\s+/g, " ");
        check(name, `${tag}: a rule no runner meets is warned of`, await until(async () => /no runner connected now meets rule \d+ \(plan9\)/.test(await rulesText())), (await rulesText()).slice(-300));
        await serverRules.getByRole("listitem").last().getByRole("button", { name: "Remove" }).click();
        await serverRules.getByRole("button", { name: "Save the rules" }).click();
        check(name, `${tag}: and taken out`, await until(async () => /Saved/.test(await rulesText()) && !/plan9/.test(await rulesText())), (await rulesText()).slice(-300));
        const issuer = (await page.locator("#ci\\.oidc, [id='ci.oidc']").first().innerText().catch(() => "")).replace(/\s+/g, " ");
        check(name, `${tag}: the issuer and its keys`, /\/oidc/.test(issuer) && /signing now/.test(issuer), issuer.slice(0, 200));
        // How it's reached (W10-R12): not from the internet at 127.0.0.1; its own address; published.
        check(name, `${tag}: how the issuer is reached`, /From the internet No: its URL is this network's/.test(issuer) && /Its own address 127\.0\.0\.1:\d+/.test(issuer) && /Published \//.test(issuer), issuer.slice(0, 500));
        const oidcGroup = page.locator("[id='ci.oidc']").first();
        await oidcGroup.getByRole("button", { name: "Publish", exact: true }).click();
        check(name, `${tag}: published by hand`, await until(async () => /Published to \//.test(await oidcGroup.innerText())), (await oidcGroup.innerText().catch(() => "")).replace(/\s+/g, " ").slice(-300));
        check(name, `${tag}: the CI settings fit`, (await overflow(page)) <= 0, String(await overflow(page)));
        await checkLayout(check, `${name}: ${tag}: the CI settings`, page);
        await page.screenshot({ path: `${shots}/insights-${name}-${tag}-ci-settings.png`, fullPage: true });
      }
      // The Runners page: each runner's executors, as it last probed (W21-R08).
      if (who === "lead") {
        await page.goto(`${cfg.base}/runners`);
        const fleet = page.locator("table.data-table").first();
        await fleet.getByRole("columnheader", { name: "Executors" }).waitFor({ timeout: 30000 });
        const col = await fleet.locator("thead th").evaluateAll((ths) => ths.findIndex((t) => t.textContent.trim() === "Executors"));
        const cell = fleet.locator("tbody tr").first().locator("td").nth(col);
        check(name, `${tag}: the runner's executors`, await until(async () => /\bhost\b/.test(await cell.innerText())), await cell.innerText().catch(() => ""));
        check(name, `${tag}: the Runners page fits`, (await overflow(page)) <= 0, String(await overflow(page)));
        await checkLayout(check, `${name}: ${tag}: the Runners page`, page);
        await page.screenshot({ path: `${shots}/insights-${name}-${tag}-runners.png`, fullPage: true });
      }
      // The runs list: words typed narrow it, facets offered with counts, a sort chosen (W21-R03).
      await page.goto(`${cfg.base}/runs`);
      const filterBox = page.getByRole("combobox", { name: "Filter runs" }).or(page.getByLabel("Filter runs")).first();
      await filterBox.waitFor({ timeout: 30000 });
      // The list, loaded: its runs' rows.
      await page.locator("table.data-table tbody tr", { hasText: "round 1" }).first().waitFor({ timeout: 30000 });
      await filterBox.fill("round 3");
      const rowsText = async () => (await page.locator("table.data-table tbody tr").allInnerTexts()).map((t) => t.replace(/\s+/g, " "));
      check(name, `${tag}: words narrow the runs`, await until(async () => {
        const rows = await rowsText();
        return rows.length >= 1 && rows.every((r) => /round 3/.test(r));
      }), (await rowsText()).join(" | ").slice(0, 300));
      await filterBox.fill("");
      await page.getByLabel("Sort").selectOption("oldest");
      check(name, `${tag}: oldest first`, await until(async () => /round 1\b/.test((await rowsText())[0] ?? "")), (await rowsText())[0] ?? "");
      // Where legs ran, facets of their own (W21-R03): the runner and its executor, each with its runs.
      await filterBox.click();
      const facetList = page.getByRole("listbox", { name: "Filter runs" });
      await facetList.waitFor({ timeout: 15000 });
      const facetText = (await facetList.innerText()).replace(/\s+/g, " ");
      check(name, `${tag}: runner and executor facets`, /Runner/i.test(facetText) && /Executor/i.test(facetText) && /host/.test(facetText), facetText.slice(0, 400));
      await page.keyboard.press("Escape");
      check(name, `${tag}: the runs page fits`, (await overflow(page)) <= 0, String(await overflow(page)));
      await checkLayout(check, `${name}: ${tag}: the runs page`, page);
      await page.screenshot({ path: `${shots}/insights-${name}-${tag}-runs.png`, fullPage: true });
      // The home page: a person of one repository sees theirs, without the server's runners (W10).
      await page.goto(`${cfg.base}/`);
      await page.getByRole("heading", { level: 1 }).first().waitFor({ timeout: 30000 });
      // Loaded: the KPIs have their numbers.
      await until(async () => /RUNS · 24H\s*\d/i.test(await page.locator("main, body").first().innerText()), 30000);
      const h1 = (await page.getByRole("heading", { level: 1 }).first().innerText()).trim();
      const home = (await page.locator("main, body").first().innerText()).replace(/\s+/g, " ");
      if (who === "dev") {
        check(name, `${tag}: dev's home is their repository's`, h1 === "acme/app" && !/Slots busy/i.test(home) && !/Server host/i.test(home), `${h1} · ${home.slice(0, 200)}`);
      } else {
        check(name, `${tag}: lead's home is the server's`, h1 !== "acme/app" && /Slots busy/i.test(home) && /Server host/i.test(home), `${h1} · ${home.slice(0, 200)}`);
      }
      check(name, `${tag}: the home page fits`, (await overflow(page)) <= 0, String(await overflow(page)));
      await checkLayout(check, `${name}: ${tag}: the home page`, page);
      await page.screenshot({ path: `${shots}/insights-${name}-${tag}-home.png`, fullPage: true });
      check(name, `${tag}: no page errors`, errors.length === 0, errors.join("; "));
      check(name, `${tag}: no failed requests`, bad.length === 0, bad.join(", "));
      await ctx.close();
    }
  } finally {
    await browser.close();
  }
}
console.log(report.join("\n"));
process.exit(failed ? 1 : 0);
