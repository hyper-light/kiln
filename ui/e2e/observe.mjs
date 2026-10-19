// Observe's UI, as a person uses it, against a live kiln server (docs/observe.md §13): each
// scenario drives the pages, then checks what they did through the API (and, for integrations,
// what a receiver was sent). It changes what it makes and removes it after.
//
//   KILN_URL=http://127.0.0.1:17170 KILN_TOKEN=$(cat <data>/admin.token) \
//   PLAYWRIGHT_CORE=/path/to/node_modules/playwright-core/index.mjs \
//   [BROWSER=chromium|webkit] [PHONE=1] [ONLY=<part of a scenario's name>] [SHOTS=<folder>] \
//   [RECEIVER=http://127.0.0.1:17199 RECEIVED=<the jsonl it keeps what it is sent in>] \
//   [LOGS_DIR=<a folder this script writes in and the server reads>] \
//   node ui/e2e/observe.mjs
//
// The server needs observe's data to look at: metrics of `app.latency.p95` by `service` and logs
// with `status:error` among them (what the seed of docs/observe.md §13 writes). playwright-core is
// not a dependency of the UI's: point PLAYWRIGHT_CORE at one, of the browsers installed.
import dgram from "node:dgram";
import fs from "node:fs";

const BASE = process.env.KILN_URL ?? "http://127.0.0.1:17170";
const TOKEN = process.env.KILN_TOKEN ?? "";
const pw = await import(process.env.PLAYWRIGHT_CORE ?? "playwright-core");
const which = process.env.BROWSER ?? "chromium";
const phone = process.env.PHONE === "1";
const run = Date.now().toString(36);

const api = async (method, path, body) => {
  const r = await fetch(BASE + path, { method, headers: { authorization: `Bearer ${TOKEN}`, "content-type": "application/json" }, body: body === undefined ? undefined : JSON.stringify(body) });
  const text = await r.text();
  return { status: r.status, body: text ? JSON.parse(text) : null };
};
const expect = (ok, what) => {
  if (!ok) throw new Error(`expected: ${what}`);
};
/** Types `text` into a field and checks it took: WebKit can drop text inserted into a field the page
 * made a moment before (no input event at all, the field left empty), so it is typed again. */
const fillIn = async (locator, text) => {
  for (let i = 0; ; i++) {
    await locator.fill(text);
    if ((await locator.inputValue()) === text) return;
    if (i >= 4) throw new Error(`the field never took ${JSON.stringify(text)}`);
    await new Promise((r) => setTimeout(r, 100));
  }
};
const until = async (what, f, ms = 15_000) => {
  const end = Date.now() + ms;
  for (;;) {
    const v = await f();
    if (v) return v;
    if (Date.now() > end) throw new Error(`not within ${ms} ms: ${what}`);
    await new Promise((r) => setTimeout(r, 250));
  }
};

const launch = which === "webkit" ? pw.webkit.launch() : pw.chromium.launch(process.env.CHROMIUM ? { executablePath: process.env.CHROMIUM } : {});
const browser = await launch;
const ctx = await browser.newContext(phone ? { viewport: { width: 390, height: 844 }, deviceScaleFactor: 2, isMobile: true, hasTouch: true } : { viewport: { width: 1440, height: 1000 } });
expect((await ctx.request.post(`${BASE}/api/auth/login`, { data: { token: TOKEN } })).ok(), "signing in with the token");
const page = await ctx.newPage();
const problems = [];
// WebKit says a same-origin request a navigation cancelled failed "due to access control checks":
// no request here crosses origins, so that is never a fault of the page's.
page.on("pageerror", (e) => !/due to access control checks/.test(e.message) && problems.push(`pageerror: ${e.message}`));
// And a fetch a navigation (a reload) cancelled "Load failed": the page goes, its requests with it.
page.on(
  "console",
  (m) => m.type() === "error" && !m.text().startsWith("Failed to load resource") && !(which === "webkit" && m.text() === "TypeError: Load failed") && problems.push(`console: ${m.text()}`),
);
page.on("response", (r) => r.status() >= 400 && problems.push(`HTTP ${r.status()} ${r.request().method()} ${r.url().replace(BASE, "")}`));
// The page's loads and navigations, said when a scenario fails (a reload loses what was typed).
const navs = [];
page.on("framenavigated", (f) => f === page.mainFrame() && navs.push(`${Date.now() % 100_000} navigated ${f.url().replace(BASE, "")}`));
page.on("load", () => navs.push(`${Date.now() % 100_000} load`));
if (process.env.TRACE) page.on("request", (r) => r.url().includes("/api/") && console.log(`  ${Date.now() % 100000} ${r.method()} ${r.url().replace(BASE, "")}`));

const scenarios = {
  /** A query explored, put on a new dashboard, the dashboard given a note and saved. */
  async "a query goes from the explorer onto a new dashboard, which is changed and saved"() {
    const query = "avg:app.latency.p95{*} by {service}";
    await page.goto(`${BASE}/observe/metrics`);
    await fillIn(page.locator("#observe-query"), query);
    await page.getByRole("button", { name: "Run", exact: true }).click();
    await page.waitForURL(/\/observe\/metrics\?q=/);
    const rows = page.locator("table.data-table tbody tr");
    await until("the series' rows", async () => (await rows.count()) === 3);
    const labels = (await rows.locator("td:first-child").allTextContents()).map((t) => t.trim()).sort();
    expect(JSON.stringify(labels) === JSON.stringify(["service:api", "service:web", "service:worker"]), `the series by service, got ${labels}`);
    await page.getByRole("button", { name: "Add to dashboard", exact: true }).click();
    const dialog = page.getByRole("dialog");
    await dialog.getByLabel("Dashboard", { exact: true }).selectOption("new");
    await fillIn(dialog.getByLabel("New dashboard's title"), `E2E ${run}`);
    await fillIn(dialog.getByLabel("Widget's title"), "Latency by service");
    await dialog.getByRole("button", { name: "Add", exact: true }).click();
    await page.waitForURL(/\/observe\/dashboards\/d[0-9a-f]+/);
    const id = decodeURIComponent(page.url().split("/").pop().split("?")[0]);
    await page.getByRole("heading", { name: `E2E ${run}` }).waitFor();
    await page.getByRole("heading", { name: "Latency by service" }).waitFor();
    let d = await api("GET", `/api/observe/v1/dashboards/${id}`);
    expect(d.status === 200 && d.body.widgets.length === 1 && d.body.widgets[0].queries[0] === query, "the dashboard kept, its widget of the query");
    // A note added in place, then saved.
    await page.getByRole("button", { name: "Edit", exact: true }).click();
    await page.getByRole("button", { name: "Add a widget", exact: true }).click();
    await page.getByRole("dialog").getByLabel("Kind").selectOption("note");
    await fillIn(page.getByRole("dialog").getByLabel("Title"), "Read me");
    await fillIn(page.locator("#widget-text"), "Latency is **p95**, by service.");
    await page.getByRole("dialog").getByRole("button", { name: "Add", exact: true }).click();
    await page.getByRole("button", { name: "Save", exact: true }).click();
    await page.getByRole("button", { name: "Edit", exact: true }).waitFor();
    await page.locator(".obs-note strong", { hasText: "p95" }).waitFor();
    d = await api("GET", `/api/observe/v1/dashboards/${id}`);
    expect(d.body.widgets.length === 2 && d.body.widgets[1].kind === "note" && d.body.widgets[1].layout.y >= d.body.widgets[0].layout.y, "two widgets, the note after the chart");
    // Deleted from its page: gone from the list.
    await page.getByRole("button", { name: "Delete the dashboard", exact: true }).click();
    await page.getByRole("dialog").getByRole("button", { name: "Delete", exact: true }).click();
    await page.waitForURL(/\/observe\/dashboards$/);
    expect((await api("GET", `/api/observe/v1/dashboards/${id}`)).status === 404, "the dashboard gone");
  },

  /** A forecast in the explorer: a disk filling a percent an hour drawn past now (the API's answer
   * forecasting it carried on), and the table's figures the data's, its last the last point's. */
  async "a forecast is drawn past now, and the explorer's figures are the data's"() {
    const metric = `e2e.forecast.disk.${run}`;
    const now = Math.floor(Date.now() / 600_000) * 600_000;
    const points = Array.from({ length: 6 * 24 + 1 }, (_, i) => {
      const t = now - (6 * 24 - i) * 600_000;
      return [t / 1000, 62 + (t - now) / 3_600_000];
    });
    expect((await api("POST", "/api/observe/v1/series", { series: [{ metric, type: "gauge", tags: ["host:db-1"], points }] })).status === 202, "writing the disk's points");
    const query = `forecast(avg:${metric}{*} by {host}, 'linear', 2)`;
    await until("the points stored", async () => (await api("POST", "/api/observe/v1/query", { query: `avg:${metric}{*}`, from: now - 3_600_000, to: now + 600_000 })).body?.series?.length === 1);
    await page.goto(`${BASE}/observe/metrics?range=1d&q=${encodeURIComponent(query)}`);
    const row = page.locator("table.data-table tbody tr").first();
    await row.waitFor({ timeout: 20_000 });
    // The data's last, 62, not the forecast's (some 74 twelve hours on).
    const last = Number((await row.locator("td").nth(1).innerText()).replace(/,/g, ""));
    expect(Math.abs(last - 62) < 0.5, `the table's last is the data's: ${last}`);
    await page.locator("canvas").first().waitFor();
    const a = await api("POST", "/api/observe/v1/query", { query, from: now - 86_400_000, to: now + 12 * 3_600_000, points: 240 });
    const s = a.body.series[0];
    const from = s.bounds?.forecast_from;
    expect(a.status === 200 && Number.isInteger(from) && from < s.values.length, `a forecast past now: ${JSON.stringify(s.bounds ?? null).slice(0, 120)}`);
    const end = s.values.at(-1);
    expect(Math.abs(end - 74) < 2 && s.bounds.lower.at(-1) < end && end < s.bounds.upper.at(-1), `carried on to ${end} in its band`);
  },

  /** Outliers in the explorer: five hosts, one running hot; `outliers()` keeps its color and greys
   * its peers (the table's dots say so), as the API's answer flags it alone. */
  async "an outlier keeps its color in the explorer, its peers greyed"() {
    const metric = `e2e.outlier.cpu.${run}`;
    const now = Math.floor(Date.now() / 60_000) * 60_000;
    const series = [40, 42, 39, 41, 85].map((level, h) => ({
      metric,
      type: "gauge",
      tags: [`host:web-${h + 1}`],
      points: Array.from({ length: 60 }, (_, i) => [(now - (59 - i) * 60_000) / 1000, level + ((i * 7 + h * 3) % 5) - 2]),
    }));
    expect((await api("POST", "/api/observe/v1/series", { series })).status === 202, "writing the points");
    const query = `outliers(avg:${metric}{*} by {host}, 'dbscan', 1)`;
    await until("the points stored", async () => (await api("POST", "/api/observe/v1/query", { query, from: now - 3_600_000, to: now + 60_000 })).body?.series?.length === 5);
    const a = await api("POST", "/api/observe/v1/query", { query, from: now - 3_600_000, to: now + 60_000 });
    const flagged = a.body.series.filter((s) => s.outlier).map((s) => s.tags.host);
    expect(JSON.stringify(flagged) === JSON.stringify(["web-5"]), `the API flags web-5 alone: ${flagged}`);
    await page.goto(`${BASE}/observe/metrics?range=1h&q=${encodeURIComponent(query)}`);
    const rows = page.locator("table.data-table tbody tr");
    await until("five rows", async () => (await rows.count()) === 5, 20_000);
    const dots = await rows.evaluateAll((trs) => trs.map((tr) => [tr.querySelector("td")?.textContent?.trim() ?? "", getComputedStyle(tr.querySelector("td span") ?? tr).backgroundColor]));
    const grey = dots.filter(([label]) => !label.includes("web-5")).map(([, c]) => c);
    const hot = dots.find(([label]) => label.includes("web-5"))?.[1];
    expect(new Set(grey).size === 1 && hot && !grey.includes(hot), `web-5 in its color, its peers alike in grey: ${JSON.stringify(dots)}`);
  },

  /** A calendar rollup in the explorer: three days of a count of one an hour, summed by New York's
   * days; the API's buckets are local midnights, and the page says it is a day a bucket, its most a
   * whole day's hours. */
  async "a calendar rollup is drawn by the days of its timezone"() {
    const metric = `e2e.calendar.hits.${run}`;
    const now = Math.floor(Date.now() / 3_600_000) * 3_600_000;
    const points = Array.from({ length: 72 }, (_, i) => [(now - (71 - i) * 3_600_000) / 1000, 1]);
    expect((await api("POST", "/api/observe/v1/series", { series: [{ metric, type: "count", tags: [], points }] })).status === 202, "writing the points");
    const query = `sum:${metric}{*}.rollup(sum, daily, 'America/New_York')`;
    const ask = () => api("POST", "/api/observe/v1/query", { query, from: now - 7 * 86_400_000, to: now + 3_600_000 });
    await until("the points stored", async () => ((await ask()).body?.series?.[0]?.values ?? []).reduce((a, v) => a + (v ?? 0), 0) === 72);
    const { grid } = (await ask()).body;
    const local = grid.times.map((t) => (t % 86_400_000) / 3_600_000);
    expect(grid.calendar === "daily" && grid.times.length === grid.n && local.every((h) => h === 4 || h === 5), `New York's midnights: ${local}`);
    await page.goto(`${BASE}/observe/metrics?range=7d&q=${encodeURIComponent(query)}`);
    await page.getByText("every day", { exact: false }).first().waitFor({ timeout: 20_000 });
    const max = Number((await page.locator("table.data-table tbody tr td").nth(4).textContent()).trim());
    expect([23, 24, 25].includes(max), `a whole day's hours at the most: ${max}`);
  },

  /** A distribution on a dashboard: two hosts' values, a heatmap of them and their bars, the bars'
   * p50 and p99 marked; a cell tapped says its range and count. */
  async "a distribution is drawn as a heatmap and as bars, its percentiles marked"() {
    const metric = `e2e.dist.${run}`;
    const now = Date.now();
    // Six points of 200 values a host over the last half hour (deterministic, a host each level).
    const points = (level) =>
      Array.from({ length: 6 }, (_, k) => [(now - (30 - 5 * k) * 60_000) / 1000, Array.from({ length: 200 }, (_, i) => Math.exp(level + Math.sin(i * 12.9898 + k * 7.233) * 0.8))]);
    const posted = await api("POST", "/api/observe/v1/distribution_points", {
      series: [
        { metric, host: "a", points: points(-3) },
        { metric, host: "b", points: points(-2) },
      ],
    });
    expect(posted.status === 202, `posting the values: ${posted.status}`);
    // Every bucket the hour has: a grid begins at a multiple of its step, so an hour can straddle two.
    await until(
      "all 2,400 counted",
      async () =>
        ((await api("POST", "/api/observe/v1/query", { query: `count:${metric}{*}`, from: now - 3_600_000, to: now + 60_000, points: 1 })).body?.series?.[0]?.values ?? []).reduce(
          (a, v) => a + (v ?? 0),
          0,
        ) === 2400,
    );
    const widget = (id, title, kind, query, y) => ({ id, title, kind, queries: [query], search: "", text: "", layout: { x: 0, y, w: 12, h: 4 }, options: { unit: "s" } });
    const d = await api("POST", "/api/observe/v1/dashboards", {
      title: `e2e distribution ${run}`,
      description: "",
      template_variables: [],
      widgets: [widget("h", "Its heatmap", "heatmap", `p50:${metric}{*}`, 0), widget("d", "Its distribution", "distribution", `p50:${metric}{*} by {host}`, 4)],
    });
    expect(d.status === 200 || d.status === 201, `the dashboard: ${d.status}`);
    const id = d.body.id;
    await page.goto(`${BASE}/observe/dashboards/${id}?range=1h`);
    const heat = page.getByRole("img", { name: /^A heatmap: \d+ rows of values/ });
    await heat.waitFor({ timeout: 20_000 });
    const bars = page.getByRole("img", { name: /^A distribution of 2,400 values, p50 .+, p99 .+/ });
    await bars.waitFor({ timeout: 20_000 });
    // The busiest cell is a tap away from saying what it holds.
    const box = await heat.boundingBox();
    await heat.click({ position: { x: Math.max(70, box.width * 0.95), y: box.height * 0.5 } });
    await page.locator(".obs-tooltip").first().waitFor({ timeout: 5_000 });
    const said = await page.locator(".obs-tooltip").first().textContent();
    expect(/ – .+: [\d,]+$/.test(said?.trim() ?? ""), `a cell's range and count: ${said}`);
    await api("DELETE", `/api/observe/v1/dashboards/${id}`);
  },

  /** kiln's own dashboard narrowed to one host by its template variable. */
  async "a template variable narrows every widget to its value"() {
    const hosts = (await api("GET", "/api/observe/v1/tags?key=host")).body.values;
    expect(hosts.length > 0, "a host to choose");
    await page.goto(`${BASE}/observe/dashboards/builtin:hosts`);
    await page.getByLabel("$host").selectOption(hosts[0]);
    await page.waitForURL(new RegExp(`var\\.host=${encodeURIComponent(hosts[0])}`));
    // A copy changes; kiln's own does not.
    expect((await page.getByRole("button", { name: "Edit", exact: true }).count()) === 0, "kiln's own dashboard has no Edit");
    await page.getByRole("button", { name: "Copy to change", exact: true }).click();
    await page.waitForURL(/\/observe\/dashboards\/d[0-9a-f]+/);
    const id = decodeURIComponent(page.url().split("/").pop().split("?")[0]);
    const d = await api("GET", `/api/observe/v1/dashboards/${id}`);
    expect(d.body.title === "Hosts (copy)" && !d.body.builtin && d.body.widgets.length > 0, "an editable copy");
    // Off its page first: a page showing a dashboard deleted elsewhere keeps what it shows.
    await page.goto(`${BASE}/observe/dashboards`);
    await page.getByRole("link", { name: "Hosts (copy)" }).first().waitFor();
    await api("DELETE", `/api/observe/v1/dashboards/${id}`);
  },

  /** Logs searched, narrowed by a facet, a record opened and one of its attributes searched. */
  async "a log search narrows by a facet and by an attribute of a record"() {
    // Records of its own (whatever else the server holds): two services, errors and not, each with a route.
    const source = `e2e-${run}`;
    const routes = ["/api/runs", "/api/jobs", "/login"];
    const logs = Array.from({ length: 30 }, (_, i) => ({
      message: i % 2 ? `GET ${routes[i % 3]} 200` : `POST ${routes[i % 3]} 500: deadline exceeded`,
      status: i % 2 ? "info" : "error",
      service: i % 3 ? `${source}-api` : `${source}-web`,
      hostname: "e2e",
      source,
      attributes: { route: routes[i % 3] },
    }));
    expect((await api("POST", "/api/observe/v1/logs", logs)).status === 202, "posting the records");
    const search = `status:error source:${source}`;
    await until(
      "the records searchable",
      async () => (await api("POST", "/api/observe/v1/logs/search", { query: search, from: Date.now() - 600_000, to: Date.now(), limit: 20 })).body?.records?.length === 15,
    );
    await page.goto(`${BASE}/observe/logs`);
    await fillIn(page.locator("#observe-search"), search);
    await page.getByRole("button", { name: "Search", exact: true }).click();
    await page.waitForURL(/q=status%3Aerror/);
    const service = page.locator('aside[aria-label="Facets"] section[aria-label="service"] button').first();
    await service.waitFor();
    const name = (await service.locator("span.truncate").textContent()).trim();
    await service.click();
    await page.waitForURL(new RegExp(`service%3A${name}`));
    const first = page.locator('section[aria-label="Records"] li > button').first();
    await first.waitFor();
    // Every record shown is an error of that service (each one's dot says its status).
    const statuses = await page.locator('section[aria-label="Records"] li > button > span[aria-label]').evaluateAll((els) => els.map((e) => e.getAttribute("aria-label")));
    expect(statuses.length > 0 && statuses.every((s) => s === "error"), `errors only, got ${[...new Set(statuses)]}`);
    await first.click();
    const route = page.locator('section[aria-label="Records"] th', { hasText: "@route" }).locator("xpath=following-sibling::td//button");
    const value = (await route.textContent()).trim();
    await route.click();
    await page.waitForURL(new RegExp(`%40route%3A${encodeURIComponent(value).replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}`));
    // The search the page made is one the API answers the same.
    const q = new URL(page.url()).searchParams.get("q");
    const viaApi = await api("POST", "/api/observe/v1/logs/search", { query: `(${q}) -source:kiln.event`, from: Date.now() - 3_600_000, to: Date.now(), limit: 1000 });
    const expected = logs.filter((l) => l.status === "error" && l.service === name && l.attributes.route === value).length;
    expect(
      viaApi.status === 200 && viaApi.body.records.length === expected && viaApi.body.records.every((r) => r.status === "error" && r.service === name && r.attributes.route === value),
      "the API agrees with the page's search",
    );
  },

  /** A composite made from the form of two monitors grouped by host: it alerts the hosts both alert
   * on and no other, lists what it reads, and a monitor it reads is not deleted while it does. */
  async "a composite is made of two monitors, alerts what they share, and keeps what it reads"() {
    const made = [];
    try {
      for (const [x, hosts] of [
        ["a", ["web01", "web02", "web03"]],
        ["b", ["web02", "web03", "web04"]],
      ]) {
        const m = await api("POST", "/api/observe/v1/monitors", {
          name: `E2E ${x} ${run}`,
          query: `max(last_1m):max:e2e.composite.${x}.${run}{*} by {host} > 10`,
          options: { every: 10, thresholds: { critical: 10 } },
        });
        expect(m.status === 201, `monitor ${x}: ${JSON.stringify(m.body)}`);
        made.push(m.body.id);
        const now = Date.now();
        const series = hosts.map((h) => ({ metric: `e2e.composite.${x}.${run}`, type: "gauge", tags: [`host:${h}`], points: [0, 1, 2, 3].map((i) => [(now - 30_000 + i * 10_000) / 1000, 20]) }));
        expect((await api("POST", "/api/observe/v1/series", { series })).status === 202, "writing its points");
      }
      await page.goto(`${BASE}/observe/monitors/new`);
      await fillIn(page.getByLabel("Name", { exact: true }), `E2E both ${run}`);
      await page.getByRole("group", { name: "Kind", exact: true }).getByRole("button", { name: "Composite", exact: true }).click();
      await page.getByLabel("Monitor a", { exact: true }).selectOption({ label: `E2E a ${run}` });
      await page.getByLabel("Monitor b", { exact: true }).selectOption({ label: `E2E b ${run}` });
      await fillIn(page.getByLabel("Alerts when", { exact: true }), "a && b");
      await page.getByRole("button", { name: "Create", exact: true }).click();
      await page.waitForURL(/\/observe\/monitors\/m[0-9a-f]+$/);
      const id = page.url().split("/").pop();
      made.unshift(id);
      const reads = page.getByRole("region", { name: "What it reads" });
      await reads.getByText(`E2E a ${run}`).waitFor();
      await reads.getByText(`E2E b ${run}`).waitFor();
      const c = await api("GET", `/api/observe/v1/monitors/${id}`);
      expect(
        c.body.type === "composite" && c.body.composite.expression === "a && b" && c.body.query === `${made[1]} && ${made[2]}`,
        `the composite kept as made: ${JSON.stringify(c.body).slice(0, 300)}`,
      );
      // Its groups: the hosts both alert on.
      const host = (g) => g.group[0]?.[1];
      await until(
        "the composite alerting web02 and web03 alone",
        async () => {
          const m = (await api("GET", `/api/observe/v1/monitors/${id}`)).body;
          const alerting = m.groups
            .filter((g) => g.state === "ALERT")
            .map(host)
            .sort();
          return m.groups.length === 2 && JSON.stringify(alerting) === JSON.stringify(["web02", "web03"]);
        },
        60_000,
      );
      await page.reload();
      await page.getByRole("region", { name: "Groups" }).getByText("host:web02").waitFor();
      // A monitor it reads is not deleted: the page says why, and it stays.
      await page.goto(`${BASE}/observe/monitors/${made[1]}`);
      await page.getByRole("button", { name: "Delete the monitor", exact: true }).click();
      await page.getByRole("dialog").getByRole("button", { name: "Delete", exact: true }).click();
      await page
        .getByText(/composite monitors read it/)
        .first()
        .waitFor();
      problems.length = 0; // The refusal's 409 is the point.
      expect((await api("GET", `/api/observe/v1/monitors/${made[1]}`)).status === 200, "the monitor it reads is still there");
    } finally {
      for (const id of made) await api("DELETE", `/api/observe/v1/monitors/${id}`);
    }
  },

  /** A monitor made from the form, muted, unmuted and deleted. */
  async "a monitor is made, muted, unmuted and deleted"() {
    await page.goto(`${BASE}/observe/monitors/new?query=${encodeURIComponent("avg(last_5m):avg:app.latency.p95{*} by {service} > 1000000")}`);
    await fillIn(page.getByLabel("Name", { exact: true }), `E2E latency ${run}`);
    await fillIn(page.getByLabel("Warning at", { exact: true }), "900000");
    await page.getByRole("button", { name: "Create", exact: true }).click();
    await page.waitForURL(/\/observe\/monitors\/m[0-9a-f]+$/);
    const id = page.url().split("/").pop();
    let m = await api("GET", `/api/observe/v1/monitors/${id}`);
    expect(m.status === 200 && m.body.name === `E2E latency ${run}` && m.body.options.thresholds.warning === 900000, "the monitor kept as made");
    await page.getByRole("button", { name: "Mute", exact: true }).click();
    await page.getByText("· muted").first().waitFor();
    m = await api("GET", `/api/observe/v1/monitors/${id}`);
    expect(m.body.muted_until > Date.now(), "muted on the server");
    await page.getByRole("button", { name: "Unmute", exact: true }).click();
    await page.getByRole("button", { name: "Mute", exact: true }).waitFor();
    m = await api("GET", `/api/observe/v1/monitors/${id}`);
    expect(!(m.body.muted_until > Date.now()), "unmuted on the server");
    await page.getByRole("button", { name: "Delete the monitor", exact: true }).click();
    await page.getByRole("dialog").getByRole("button", { name: "Delete", exact: true }).click();
    await page.waitForURL(/\/observe\/monitors$/);
    expect((await api("GET", `/api/observe/v1/monitors/${id}`)).status === 404, "the monitor gone");
  },

  /** A signed webhook added, tested (the receiver gets a signed notice), and removed. */
  async "an integration is added, tested and removed"() {
    const receiver = process.env.RECEIVER;
    if (!receiver) return "skipped: no RECEIVER";
    const name = `e2e-${run}`;
    await page.goto(`${BASE}/observe/integrations`);
    await page.getByRole("button", { name: "Add integration", exact: true }).click();
    const dialog = page.getByRole("dialog");
    await dialog.getByLabel("Kind").selectOption("webhook");
    await fillIn(dialog.getByLabel("Name"), name);
    await fillIn(dialog.getByLabel("URL"), `${receiver}/${name}`);
    await fillIn(dialog.getByLabel("Signing secret (optional)"), "e2e-secret");
    await dialog.getByRole("button", { name: "Add", exact: true }).click();
    const row = page.locator("tr", { hasText: name });
    await row.waitFor();
    const before = fs.existsSync(process.env.RECEIVED) ? fs.readFileSync(process.env.RECEIVED, "utf8").split("\n").filter(Boolean).length : 0;
    await row.getByRole("button", { name: "Test", exact: true }).click();
    const got = await until("the test notice at the receiver", () => {
      const lines = fs.existsSync(process.env.RECEIVED) ? fs.readFileSync(process.env.RECEIVED, "utf8").split("\n").filter(Boolean) : [];
      return lines
        .slice(before)
        .map((l) => JSON.parse(l))
        .find((x) => x.path === `/${name}`);
    });
    expect(/^sha256=[0-9a-f]{64}$/.test(got.sig ?? ""), `a signed notice, got ${got.sig}`);
    await row.getByRole("button", { name: `Remove ${name}`, exact: true }).click();
    await page.getByRole("dialog").getByRole("button", { name: "Remove", exact: true }).click();
    await until("the integration gone", async () => !(await api("GET", "/api/observe/v1/integrations")).body.integrations.some((i) => i.name === name));
  },
  /** A Prometheus endpoint (the server's own /metrics) added from the form, tested, scraped, and removed. */
  async "a scrape target is tested, added, read, and removed"() {
    const name = `e2e-s-${run}`;
    await page.goto(`${BASE}/observe/integrations`);
    await page.getByRole("button", { name: "Add target", exact: true }).click();
    const dialog = page.getByRole("dialog");
    await fillIn(dialog.getByLabel("Name"), name);
    await fillIn(dialog.getByLabel("Endpoint"), `${BASE}/metrics`);
    await fillIn(dialog.getByLabel("Every (seconds)"), "2");
    await fillIn(dialog.getByLabel("Timeout (seconds)"), "1");
    await dialog.getByRole("button", { name: "Test", exact: true }).click();
    const said = await dialog.getByRole("status").textContent({ timeout: 15_000 });
    expect(/^Reads [1-9][\d,]* samples of [1-9][\d,]* metrics \(Prometheus text\)/.test(said ?? ""), `the test reads the server's own: ${said}`);
    await dialog.getByRole("button", { name: "Add", exact: true }).click();
    // Read by the server, up, as the API says and the page shows.
    await until(
      "the target up",
      async () => (await api("GET", "/api/observe/v1/scrapes")).body.scrapes.find((t) => t.name === name)?.machines?.some((m) => m.machine === "server" && m.state.up === true),
      30_000,
    );
    const row = page.locator("tr", { hasText: name });
    await until("the row says Up", async () => (await row.textContent())?.includes("Up"), 30_000);
    const up = await api("POST", "/api/observe/v1/query", { query: `max:up{job:${name}}`, from: Date.now() - 60_000, to: Date.now() + 1000, step: 1000 });
    expect(
      up.body.series?.[0]?.values?.some((v) => v === 1),
      `up 1 for job:${name}`,
    );
    await row.getByRole("button", { name: `Remove ${name}`, exact: true }).click();
    await page.getByRole("dialog").getByRole("button", { name: "Remove", exact: true }).click();
    await until("the target gone", async () => !(await api("GET", "/api/observe/v1/scrapes")).body.scrapes.some((t) => t.name === name));
  },
  /** A search's patterns: the seed's message shapes grouped, and a pattern a click from its records. */
  async "a search's records group into patterns, and a pattern narrows the search to its own"() {
    // Records of its own, of three shapes.
    const source = `e2e-p-${run}`;
    const shapes = [(i) => `connection refused by db-${i % 4}:${5400 + i}`, (i) => `GET /api/runs/${1000 + i} 200 in ${i + 3} ms`, (i) => `job ${7000 + i} queued`];
    const logs = Array.from({ length: 45 }, (_, i) => ({ message: shapes[i % 3](i), status: i % 3 ? "info" : "error", service: "e2e", hostname: "e2e", source }));
    expect((await api("POST", "/api/observe/v1/logs", logs)).status === 202, "posting the records");
    await until(
      "the records searchable",
      async () => (await api("POST", "/api/observe/v1/logs/search", { query: `source:${source}`, from: Date.now() - 600_000, to: Date.now(), limit: 100 })).body?.records?.length === 45,
    );
    await page.goto(`${BASE}/observe/logs?view=patterns&q=${encodeURIComponent(`source:${source}`)}`);
    const rows = page.getByRole("region", { name: "Patterns" }).getByRole("listitem");
    await until("the patterns", async () => (await rows.count()) >= 3, 20_000);
    const refused = rows.filter({ hasText: "connection refused by db-" });
    await refused.first().waitFor();
    await refused.first().getByRole("button").click();
    await page.waitForURL((u) => !u.searchParams.has("view") && (u.searchParams.get("q") ?? "").includes('"refused"'));
    const q = new URL(page.url()).searchParams.get("q");
    await page.getByRole("region", { name: "Records" }).waitFor();
    const to = Date.now();
    const found = await api("POST", "/api/observe/v1/logs/search", { query: `-source:kiln.event (${q})`, from: to - 3_600_000, to, limit: 200 });
    expect(found.status === 200 && found.body.records.length > 0, `the narrowed search finds records: ${JSON.stringify(found.body).slice(0, 200)}`);
    expect(
      found.body.records.every((r) => /^connection refused by db-\d+:\d+$/.test(r.message)),
      `every record the pattern narrows to is of it: ${found.body.records.map((r) => r.message).slice(0, 5)}`,
    );
  },

  /** A search followed live: what is posted after it opens comes, the newest on top, and what it does
   * not match never; paused, what comes waits (said how many), resuming shows it; opening a record
   * pauses. */
  async "a search followed live shows each record as it comes, and holds them while paused"() {
    const tag = `live-${run}`;
    await page.goto(`${BASE}/observe/logs?view=live&q=${encodeURIComponent(`service:${tag}`)}`);
    const region = page.getByRole("region", { name: "Live tail" });
    await region.getByText(/^Live · /).waitFor({ timeout: 15_000 });
    const post = async (i, service = tag) => {
      const r = await api("POST", "/api/observe/v1/logs", [{ message: `${tag} record ${i}`, service, hostname: "e2e", source: "e2e", status: i % 3 === 0 ? "error" : "info" }]);
      expect(r.status === 202, `posting record ${i}: ${r.status}`);
    };
    for (let i = 0; i < 3; i++) await post(i);
    await post(99, `${tag}-other`);
    const rows = region.getByRole("listitem");
    await until("the three records", async () => (await rows.count()) === 3);
    expect((await rows.first().innerText()).includes(`${tag} record 2`), "the newest on top");
    await page.waitForTimeout(1_000);
    expect(!(await region.innerText()).includes("record 99"), "another service's record never comes");
    await region.getByRole("button", { name: "Pause", exact: true }).click();
    for (let i = 3; i < 5; i++) await post(i);
    await region.getByText("Paused · 2 new").waitFor();
    expect((await rows.count()) === 3, "paused, the list stays as it was");
    await region.getByRole("button", { name: "Resume", exact: true }).click();
    await until("the records held while paused", async () => (await rows.count()) === 5);
    expect((await rows.first().innerText()).includes(`${tag} record 4`), "resumed, the newest on top");
    // Opening a record pauses the tail, so it stays where it is.
    await rows.nth(1).getByRole("button").first().click();
    await region.getByRole("button", { name: "Resume", exact: true }).waitFor();
    expect((await rows.nth(1).getByRole("button").first().getAttribute("aria-expanded")) === "true", "the record opened");
  },

  /**
   * A cloud added from the form (a key that is not one refused; the server's own credentials tested,
   * the test saying why they are not there), and one the API made against a stand-in for CloudWatch
   * shown pulling, edited without its secret (kept: the next pull is signed by its key) and given
   * log groups and a filter (the filter's field there once groups are), pulled now, its log records
   * counted, and both removed.
   */
  async "a cloud is added, tested, edited keeping its secret, and removed"() {
    const http = await import("node:http");
    // CloudWatch as far as a pull asks it: one metric, a point a minute.
    const seen = [];
    const cw = http.createServer((req, res) => {
      let body = "";
      req.on("data", (c) => (body += c));
      req.on("end", () => {
        const target = req.headers["x-amz-target"] ?? "";
        seen.push({ target, auth: req.headers.authorization ?? "" });
        const q = JSON.parse(body || "{}");
        const minutes = [];
        for (let t = Math.ceil((q.StartTime ?? 0) / 60) * 60; t < (q.EndTime ?? 0); t += 60) minutes.push(t);
        // CloudWatch Logs, as far as a pull asks it: a group, and three events matching ERROR.
        if (target.startsWith("Logs_20140328.")) {
          const now = Date.now();
          const logs = target.endsWith("DescribeLogGroups")
            ? { logGroups: [{ logGroupName: "/aws/lambda/e2e" }] }
            : { events: q.filterPattern === "ERROR" ? [0, 1, 2].map((i) => ({ eventId: `e${i}`, logStreamName: "s", timestamp: now - 60_000 + i, message: `[ERROR] e2e ${i} failed` })) : [] };
          res.writeHead(200, { "content-type": "application/x-amz-json-1.1" });
          res.end(JSON.stringify(logs));
          return;
        }
        const out = target.endsWith("ListMetrics")
          ? { Metrics: [{ Namespace: "AWS/EC2", MetricName: "CPUUtilization", Dimensions: [{ Name: "InstanceId", Value: "i-e2e" }] }] }
          : {
              MetricDataResults: (q.MetricDataQueries ?? []).map((m) => ({ Id: m.Id, Timestamps: minutes, Values: minutes.map(() => (m.MetricStat.Stat === "Sum" ? 42 : 1)), StatusCode: "Complete" })),
            };
        res.writeHead(200, { "content-type": "application/x-amz-json-1.0" });
        res.end(JSON.stringify(out));
      });
    });
    await new Promise((r) => cw.listen(0, "127.0.0.1", r));
    const endpoint = `http://127.0.0.1:${cw.address().port}/cw/{region}/`;
    const name = `e2e-${run}`;
    const rowOf = (n) => page.locator("table.data-table tbody tr").filter({ has: page.getByText(n, { exact: true }) });
    const clouds = async () => (await api("GET", "/api/observe/v1/clouds")).body.clouds;
    try {
      await page.goto(`${BASE}/observe/integrations`);
      await page.getByRole("button", { name: "Add cloud", exact: true }).click();
      const add = page.getByRole("dialog");
      await add.getByLabel("Cloud", { exact: true }).selectOption("gcp");
      await fillIn(add.getByLabel("Name", { exact: true }), `${name}-g`);
      await fillIn(add.getByLabel("Service account key (JSON)"), "not a key");
      await add.getByText("Not a key file's JSON.").waitFor();
      expect(await add.getByRole("button", { name: "Add", exact: true }).isDisabled(), "a key that is not one: Add is off");
      await add.getByLabel("Credentials", { exact: true }).selectOption("server");
      await add.getByRole("button", { name: "Test", exact: true }).click();
      await add
        .getByRole("status")
        .getByText(/no Google credentials/)
        .waitFor({ timeout: 20_000 });
      await add.getByRole("button", { name: "Add", exact: true }).click();
      await rowOf(`${name}-g`).waitFor();
      expect(
        (await clouds()).some((c) => c.name === `${name}-g` && c.kind === "gcp" && c.service_account === undefined),
        "kept, with the server's own credentials",
      );

      const made = await api("PUT", `/api/observe/v1/clouds/${name}`, {
        kind: "aws",
        regions: ["us-east-1"],
        namespaces: ["AWS/EC2"],
        access_key_id: "AKIDE2E",
        secret_access_key: `secret-${run}`,
        endpoint,
        logs_endpoint: endpoint.replace("/cw/", "/logs/"),
        interval_s: 300,
      });
      expect(made.status === 204, `the cloud made, got ${made.status}`);
      const row = rowOf(name);
      await until("shown pulling, its series counted", async () => /Pulling/.test((await row.textContent()) ?? "") && /3 series/.test((await row.textContent()) ?? ""), 30_000);
      await row.getByRole("button", { name: `Edit ${name}`, exact: true }).click();
      const edit = page.getByRole("dialog");
      await edit.getByText("Kept. Type a new one to replace it.").waitFor();
      await fillIn(edit.getByLabel("Every (seconds)"), "120");
      expect((await edit.getByLabel("Log filter (optional)").count()) === 0, "no filter's field without log groups");
      await fillIn(edit.getByLabel("Log groups", { exact: true }), "/aws/lambda/*");
      await fillIn(edit.getByLabel("Log filter (optional)"), "ERROR");
      await edit.getByRole("button", { name: "Save", exact: true }).click();
      const after = await until("saved", async () => (await clouds()).find((c) => c.name === name && c.interval_s === 120));
      expect(after.secret_access_key === "…" && after.endpoint === endpoint && after.access_key_id === "AKIDE2E", `its secret, key and endpoint kept: ${JSON.stringify(after)}`);
      expect(
        JSON.stringify(after.log_groups) === JSON.stringify(["/aws/lambda/*"]) && after.log_filter === "ERROR" && after.logs_endpoint === endpoint.replace("/cw/", "/logs/"),
        `its log groups and filter saved, its logs' endpoint kept: ${JSON.stringify(after)}`,
      );
      const calls = seen.length;
      await row.getByRole("button", { name: `Pull ${name} now`, exact: true }).click();
      await until("a pull after the change, signed by its key", async () => seen.length > calls && seen.slice(calls).every((s) => s.auth.includes("Credential=AKIDE2E/")));
      await until("pulling still", async () => (await clouds()).find((c) => c.name === name)?.status.state === "ok");
      await until("its log records taken, once each", async () => (await clouds()).find((c) => c.name === name)?.status.records_total === 3, 30_000);
      if (!phone) await until("its log records counted in its row", async () => /3 log records/.test((await row.textContent()) ?? ""), 30_000);
      for (const n of [name, `${name}-g`]) {
        await rowOf(n)
          .getByRole("button", { name: `Remove ${n}`, exact: true })
          .click();
        await page.getByRole("dialog").getByRole("button", { name: "Remove", exact: true }).click();
        await until(`${n} gone`, async () => !(await clouds()).some((c) => c.name === n));
      }
    } finally {
      cw.close();
      for (const n of [name, `${name}-g`]) await api("DELETE", `/api/observe/v1/clouds/${n}`);
    }
  },

  /** An Azure integration's event hubs: its stream's failing shown in its row (nothing answers at
   * its endpoint), its hubs and kept connection string in the form, a hub added, the connection
   * string given up for the integration's credentials, and removed. */
  async "an Azure integration's event hubs are shown, kept and changed"() {
    const name = `e2e-hubs-${run}`;
    const rowOf = (n) => page.locator("table.data-table tbody tr").filter({ has: page.getByText(n, { exact: true }) });
    const cloud = async () => (await api("GET", "/api/observe/v1/clouds")).body.clouds.find((c) => c.name === name);
    try {
      const made = await api("PUT", `/api/observe/v1/clouds/${name}`, {
        kind: "azure",
        metrics: false,
        event_hubs: {
          namespace: "kiln-e2e",
          hubs: ["insights-logs"],
          connection_string: `Endpoint=sb://kiln-e2e.servicebus.windows.net/;SharedAccessKeyName=k;SharedAccessKey=${run}`,
          endpoint: "127.0.0.1:9",
          tls: false,
        },
      });
      expect(made.status === 204, `the cloud made, got ${made.status}`);
      await page.goto(`${BASE}/observe/integrations`);
      const row = rowOf(name);
      await until("its hub's failing shown", async () => /Failing/.test((await row.textContent()) ?? "") && /insights-logs:/.test((await row.textContent()) ?? ""), 30_000);
      if (!phone) await until("what it reads", async () => /no metrics · 1 event hub of kiln-e2e/.test((await row.textContent()) ?? ""));
      await row.getByRole("button", { name: `Edit ${name}`, exact: true }).click();
      const edit = page.getByRole("dialog");
      expect((await edit.getByLabel("Metrics", { exact: true }).inputValue()) === "no", "metrics not read");
      expect((await edit.getByLabel("Subscriptions").count()) === 0, "no subscriptions' field without metrics");
      expect((await edit.getByLabel("Event Hubs namespace").inputValue()) === "kiln-e2e", "its namespace");
      await edit.getByText("Kept. Type a new one to replace it.").waitFor();
      await fillIn(edit.getByLabel("Event hubs", { exact: true }), "insights-logs, insights-activity-logs");
      await edit.getByRole("button", { name: "Save", exact: true }).click();
      const after = await until("saved", async () => {
        const c = await cloud();
        return c?.event_hubs?.hubs?.length === 2 ? c : undefined;
      });
      expect(
        after.event_hubs.connection_string === "…" && after.event_hubs.endpoint === "127.0.0.1:9" && after.event_hubs.tls === false && after.metrics === false,
        `its connection string, endpoint and TLS kept: ${JSON.stringify(after)}`,
      );
      await row.getByRole("button", { name: `Edit ${name}`, exact: true }).click();
      const again = page.getByRole("dialog");
      await again.getByLabel("Event hubs read with").selectOption("integration");
      await again.getByText("They need the Azure Event Hubs Data Receiver role.").waitFor();
      await again.getByRole("button", { name: "Save", exact: true }).click();
      await until("its connection string given up", async () => {
        const c = await cloud();
        return c?.event_hubs && c.event_hubs.connection_string === undefined;
      });
      await row.getByRole("button", { name: `Remove ${name}`, exact: true }).click();
      await page.getByRole("dialog").getByRole("button", { name: "Remove", exact: true }).click();
      await until(`${name} gone`, async () => !(await cloud()));
    } finally {
      await api("DELETE", `/api/observe/v1/clouds/${name}`);
    }
  },

  /** A log source added from the form: tried on the server (its file matched, its last lines read
   * as records, a token masked), kept, read (its records found by a search, once each, masked), and
   * removed. */
  async "a log source is tried, added, collected and removed"() {
    const dir = process.env.LOGS_DIR;
    if (!dir) return "skipped: no LOGS_DIR";
    const folder = `${dir}/${run}`;
    fs.mkdirSync(folder, { recursive: true });
    const lines = Array.from({ length: 30 }, (_, i) => `${JSON.stringify({ level: i % 10 === 0 ? "error" : "info", msg: `checkout ${i} token=abc${i}`, seq: i })}\n`);
    fs.writeFileSync(`${folder}/app.log`, lines.join(""));
    const name = `e2e-logs-${run}`;
    try {
      await page.goto(`${BASE}/observe/integrations`);
      await page.getByRole("button", { name: "Add log source", exact: true }).click();
      const d = page.getByRole("dialog");
      await fillIn(d.getByLabel("Name", { exact: true }), name);
      await fillIn(d.getByLabel("Paths", { exact: true }), `${folder}/*.log`);
      await d.getByLabel("Start", { exact: true }).selectOption("beginning");
      await d.getByRole("button", { name: "Add a rule", exact: true }).click();
      await d.getByLabel("Rule 1", { exact: true }).selectOption("mask_sequences");
      await fillIn(d.getByLabel("Rule 1's pattern", { exact: true }), "token=[a-z0-9]+");
      await fillIn(d.getByLabel("Rule 1's mask", { exact: true }), "token=[masked]");
      await d.getByRole("button", { name: "Test", exact: true }).click();
      const said = d.getByRole("status");
      await said.getByText(/matches 1 file/).waitFor();
      await said.getByText("checkout 29 token=[masked]", { exact: false }).waitFor();
      await d.getByRole("button", { name: "Add", exact: true }).click();
      await d.waitFor({ state: "detached" });
      const row = page.getByRole("row").filter({ hasText: name });
      await row.waitFor();
      // The server reads it at once: every line a record, once, masked.
      const found = await until(
        "its records stored",
        async () => {
          const r = await api("POST", "/api/observe/v1/logs/search", { query: `service:${name}`, from: Date.now() - 3_600_000, to: Date.now() + 60_000, limit: 100 });
          return r.body?.records?.length === 30 ? r.body.records : null;
        },
        30_000,
      );
      expect(found.every((r) => /token=\[masked\]$/.test(r.message)) && found.filter((r) => r.status === "error").length === 3, `its records: ${JSON.stringify(found.slice(0, 2))}`);
      expect(new Set(found.map((r) => r.attributes.seq)).size === 30, "each line once");
      await until("its row says it reads", async () => /Reading/.test((await row.textContent()) ?? ""), 30_000);
      await row.getByRole("button", { name: `Remove ${name}`, exact: true }).click();
      await page.getByRole("dialog").getByRole("button", { name: "Remove", exact: true }).click();
      await until("its row gone", async () => (await page.getByRole("row").filter({ hasText: name }).count()) === 0);
      const left = await api("GET", "/api/observe/v1/log_sources");
      expect(!left.body.log_sources.some((s) => s.name === name), "removed on the server");
    } finally {
      await api("DELETE", `/api/observe/v1/log_sources/${name}`);
      fs.rmSync(folder, { recursive: true, force: true });
    }
  },

  /** A syslog source added from the form, the server listening where it says (tested first): a
   * datagram sent there is a record, its program its service, its priority its status. */
  async "a syslog source listens and a datagram sent to it is a record"() {
    const port = 20_000 + Math.floor(Math.random() * 20_000);
    const name = `e2e-syslog-${run}`;
    const sock = dgram.createSocket("udp4");
    try {
      await page.goto(`${BASE}/observe/integrations`);
      await page.getByRole("button", { name: "Add log source", exact: true }).click();
      const d = page.getByRole("dialog");
      await fillIn(d.getByLabel("Name", { exact: true }), name);
      await d.getByLabel("Type", { exact: true }).selectOption("syslog");
      await fillIn(d.getByLabel("Listen on", { exact: true }), `udp://127.0.0.1:${port}`);
      await d.getByRole("button", { name: "Test", exact: true }).click();
      await d.getByRole("status").getByText("The server may listen there.").waitFor();
      await d.getByRole("button", { name: "Add", exact: true }).click();
      await d.waitFor({ state: "detached" });
      // A datagram before the server listens is lost, as UDP's are: sent until one is stored.
      const msg = `<11>1 ${new Date().toISOString()} e2e-host e2eapp-${run} 77 - - payment ${run} failed`;
      const found = await until(
        "a datagram stored",
        async () => {
          sock.send(msg, port, "127.0.0.1");
          const r = await api("POST", "/api/observe/v1/logs/search", { query: `service:e2eapp-${run}`, from: Date.now() - 600_000, to: Date.now() + 60_000, limit: 10 });
          return r.body?.records?.length > 0 ? r.body.records : null;
        },
        30_000,
      );
      const r = found[0];
      expect(
        r.message === `payment ${run} failed` && r.status === "error" && r.host === "e2e-host" && r.source === "syslog" && r.attributes["syslog.procid"] === "77",
        `the record: ${JSON.stringify(r)}`,
      );
      const row = page.getByRole("row").filter({ hasText: name });
      await until("its row says it reads", async () => /Reading/.test((await row.textContent()) ?? ""), 30_000);
      expect(/udp:\/\/127\.0\.0\.1:/.test((await row.textContent()) ?? ""), "its row says where it listens");
    } finally {
      sock.close();
      await api("DELETE", `/api/observe/v1/log_sources/${name}`);
    }
  },

  /** A pipeline made on its page: a grok parser whose rules are tried on a sample as they are
   * written, an arithmetic processor, the whole tried on a line before it is kept; then a record
   * posted is stored parsed. kiln's own nginx pipeline turned off and on from its row. */
  async "a pipeline is made, tried, kept, and parses what comes after"() {
    const name = `E2E timings ${run}`;
    const source = `e2e-shop-${run}`;
    let id;
    try {
      await page.goto(`${BASE}/observe/pipelines/new`);
      await fillIn(page.getByLabel("Name", { exact: true }), name);
      await fillIn(page.getByLabel("Filter", { exact: true }), `source:${source}`);
      await page.getByLabel("A processor to add", { exact: true }).selectOption("grok-parser");
      await page.getByRole("button", { name: "Add", exact: true }).click();
      let d = page.getByRole("dialog");
      await fillIn(d.getByLabel("Rules", { exact: true }), "r %{word:http.method} %{notSpace:http.url} took %{number:duration}ms");
      await fillIn(d.getByLabel("Samples", { exact: true }), "GET /cart took 1250ms");
      await d.getByRole("button", { name: "Try the rules on the samples", exact: true }).click();
      await d.getByRole("status").getByText("/cart", { exact: true }).waitFor();
      await d.getByRole("button", { name: "Keep", exact: true }).click();
      await d.waitFor({ state: "detached" });
      await page.getByLabel("A processor to add", { exact: true }).selectOption("arithmetic-processor");
      await page.getByRole("button", { name: "Add", exact: true }).click();
      d = page.getByRole("dialog");
      await fillIn(d.getByLabel("Expression", { exact: true }), "duration / 1000");
      await fillIn(d.getByLabel("Into", { exact: true }), "duration_s");
      await d.getByRole("button", { name: "Keep", exact: true }).click();
      await d.waitFor({ state: "detached" });
      // The whole tried on a line, before it is kept: its source guessed from the filter.
      await fillIn(page.getByLabel("Message", { exact: true }), "POST /pay took 50ms");
      expect((await page.getByLabel("Source", { exact: true }).inputValue()) === source, "the source guessed from the filter");
      await page.getByRole("button", { name: "Try it", exact: true }).click();
      await page.getByRole("status").getByText("0.05", { exact: true }).waitFor();
      await page.getByRole("button", { name: "Make the pipeline", exact: true }).click();
      await page.waitForURL(/\/observe\/pipelines\/p[0-9a-f]{16}$/);
      id = page.url().split("/").pop();
      const kept = await api("GET", `/api/observe/v1/pipelines/${id}`);
      expect(kept.body.processors.length === 2 && kept.body.filter.query === `source:${source}`, `kept as made: ${JSON.stringify(kept.body)}`);
      // A record posted after: stored parsed.
      expect((await api("POST", "/api/observe/v1/logs", [{ message: "PUT /cart took 750ms", service: "shop", hostname: "e2e", source }])).status === 202, "posting a record");
      const found = await until("the record stored parsed", async () => {
        const r = await api("POST", "/api/observe/v1/logs/search", { query: `source:${source}`, from: Date.now() - 600_000, to: Date.now() + 60_000, limit: 5 });
        return r.body?.records?.[0];
      });
      expect(found.attributes["http.method"] === "PUT" && found.attributes.duration_s === "0.75", `the record: ${JSON.stringify(found)}`);
      // kiln's own nginx pipeline: off, then on again, from its row.
      await page.goto(`${BASE}/observe/pipelines`);
      const nginx = page.getByRole("row").filter({ has: page.getByRole("link", { name: "nginx", exact: true }) });
      await nginx.getByRole("button", { name: "Turn off nginx", exact: true }).click();
      await nginx.getByText("Off", { exact: true }).waitFor();
      expect((await api("GET", "/api/observe/v1/pipelines/builtin:nginx")).body.is_enabled === false, "turned off on the server");
      await nginx.getByRole("button", { name: "Turn on nginx", exact: true }).click();
      await nginx.getByText("On", { exact: true }).waitFor();
      // Removed from its row.
      const mine = page.getByRole("row").filter({ has: page.getByRole("link", { name, exact: true }) });
      await mine.getByRole("button", { name: `Remove ${name}`, exact: true }).click();
      await page.getByRole("dialog").getByRole("button", { name: "Remove", exact: true }).click();
      await until("its row gone", async () => (await mine.count()) === 0);
      expect((await api("GET", `/api/observe/v1/pipelines/${id}`)).status === 404, "removed on the server");
    } finally {
      if (id) await api("DELETE", `/api/observe/v1/pipelines/${id}`);
      await api("PUT", "/api/observe/v1/pipelines/builtin:nginx", { name: "nginx", is_enabled: true });
    }
  },

  /** A metric from logs tried on the records of the last fifteen minutes, made, counting the
   * records that come after (as the API's query says), charted, and removed. */
  async "a metric from logs is tried, made, counts what comes after, and is removed"() {
    const service = `e2e-lm-${run}`;
    const name = `e2e.lm.${run}`;
    const post = (n, at) =>
      api(
        "POST",
        "/api/observe/v1/logs",
        Array.from({ length: n }, (_, i) => ({
          message: `GET /pay ${i % 3 === 0 ? 502 : 500}`,
          timestamp: at,
          status: "error",
          service,
          hostname: "e2e",
          attributes: { http: { status_code: i % 3 === 0 ? 502 : 500 } },
        })),
      );
    expect((await post(9, Date.now() - 60_000)).status === 202, "posting records before");
    try {
      await page.goto(`${BASE}/observe/log-metrics`);
      await page.getByRole("button", { name: "New metric", exact: true }).click();
      const d = page.getByRole("dialog");
      await fillIn(d.getByLabel("Name", { exact: true }), name);
      await fillIn(d.getByLabel("Of the records", { exact: true }), `service:${service} status:error`);
      await d.getByRole("button", { name: "Group by an attribute", exact: true }).click();
      await fillIn(d.getByLabel("Attribute 1", { exact: true }), "@http.status_code");
      await fillIn(d.getByLabel("As the tag", { exact: true }), "code");
      // Tried on what is held: the nine records, by their codes.
      await until(
        "the records stored",
        async () => (await api("POST", "/api/observe/v1/logs/search", { query: `service:${service}`, from: Date.now() - 600_000, to: Date.now() + 60_000, limit: 20 })).body?.records?.length === 9,
      );
      await d.getByRole("button", { name: "Try it", exact: true }).click();
      await d.getByRole("status").getByText("Of the last fifteen minutes: 9 records, 2 groups.", { exact: true }).waitFor();
      await d.getByRole("status").getByText("code:500", { exact: true }).waitFor();
      await d.getByRole("button", { name: "Make it", exact: true }).click();
      await d.waitFor({ state: "detached" });
      const row = page.getByRole("row").filter({ hasText: name });
      await row.waitFor();
      // Counted from the next ten seconds on: records stamped after that are its.
      const made = (await api("GET", `/api/observe/v1/log_metrics/${name}`)).body;
      const from = Math.ceil((made.modified + 1) / 10_000) * 10_000;
      await until("its first window", async () => Date.now() > from + 300, 15_000);
      expect((await post(6, Date.now())).status === 202, "posting records after");
      await until(
        "the records after counted",
        async () => {
          const a = await api("POST", "/api/observe/v1/query", { query: `sum:${name}{*} by {code}.as_count()`, from, to: Date.now() + 60_000, step: 60_000 });
          const total = (a.body?.series ?? []).reduce((t, s) => t + s.values.reduce((x, v) => x + (v ?? 0), 0), 0);
          return total === 6;
        },
        60_000,
      );
      // The page's list, read again every ten seconds, says so: its latest window's records.
      await until("its latest window on the server", async () => (await api("GET", `/api/observe/v1/log_metrics/${name}`)).body?.status?.records === 6, 30_000);
      await row
        .getByText(/^6 records, /)
        .filter({ visible: true })
        .first()
        .waitFor({ timeout: 20_000 });
      // What it computes stays: read-only once made.
      await row.getByRole("button", { name: `Edit ${name}`, exact: true }).click();
      expect((await d.getByLabel("Computes", { exact: true }).getAttribute("readonly")) !== null, "what it computes read-only");
      await d.getByRole("button", { name: "Cancel", exact: true }).click();
      await d.waitFor({ state: "detached" });
      // Charted in the explorer in a click.
      await row.getByRole("link", { name: `Chart ${name}`, exact: true }).click();
      await page.waitForURL((u) => u.pathname === "/observe/metrics" && u.searchParams.get("q") === `sum:${name}{*} by {code}.as_count()`);
      await page.goto(`${BASE}/observe/log-metrics`);
      await row.getByRole("button", { name: `Remove ${name}`, exact: true }).click();
      await page.getByRole("dialog").getByRole("button", { name: "Remove", exact: true }).click();
      await until("its row gone", async () => (await row.count()) === 0);
      expect((await api("GET", `/api/observe/v1/log_metrics/${name}`)).status === 404, "removed on the server");
    } finally {
      await api("DELETE", `/api/observe/v1/log_metrics/${name}`);
    }
  },

  /** A downtime made from the form for a monitor's group, shown on the monitor's page while it
   * holds, then canceled from its row; a recurring one made with the weekly builder, said in words. */
  async "a downtime holds a monitor's group, shows on the monitor, and is canceled"() {
    const name = `E2E dt ${run}`;
    const made = await api("POST", "/api/observe/v1/monitors", { name, query: `last(last_1m):max:e2e.dt.${run}{*} by {host} > 100`, message: "never", options: { every: 60 } });
    expect(made.status === 201, `the monitor: ${JSON.stringify(made.body)}`);
    const id = made.body.id;
    const ids = [];
    try {
      await page.goto(`${BASE}/observe/downtimes`);
      await page.getByRole("button", { name: "New downtime", exact: true }).click();
      let d = page.getByRole("dialog");
      await d.getByLabel("Monitor", { exact: true }).selectOption(id);
      await fillIn(d.getByLabel("Scope (optional)", { exact: true }), "host:a");
      const end = new Date(Date.now() + 3_600_000 - new Date().getTimezoneOffset() * 60_000).toISOString().slice(0, 16);
      await fillIn(d.getByLabel("To", { exact: true }), end);
      await d.getByRole("button", { name: "Make it", exact: true }).click();
      await d.waitFor({ state: "detached" });
      const row = page.getByRole("row").filter({ hasText: name });
      await row.getByText("Holding", { exact: true }).waitFor();
      const listed = (await api("GET", "/api/observe/v1/downtimes")).body.downtimes.filter((x) => x.monitor_identifier.monitor_id === id);
      expect(listed.length === 1 && listed[0].scope === "host:a" && listed[0].status === "active", `kept as made: ${JSON.stringify(listed)}`);
      ids.push(listed[0].id);
      // On the monitor's page: held, until when.
      await page.goto(`${BASE}/observe/monitors/${id}`);
      await page.getByRole("link", { name: /^Held by a downtime until/ }).waitFor();
      // Canceled from its row: kept, canceled.
      await page.goto(`${BASE}/observe/downtimes`);
      await row.getByRole("button", { name: "Cancel", exact: true }).click();
      await page.getByRole("dialog").getByRole("button", { name: "Cancel it", exact: true }).click();
      await row.getByText("Canceled", { exact: true }).waitFor();
      // Recurring: weekly on Saturdays and Sundays, four hours, said in words.
      await page.getByRole("button", { name: "New downtime", exact: true }).click();
      d = page.getByRole("dialog");
      await d.getByLabel("Monitor", { exact: true }).selectOption(id);
      await d.getByRole("group", { name: "When", exact: true }).getByRole("button", { name: "Recurring", exact: true }).click();
      await fillIn(d.getByLabel("For", { exact: true }), "4h");
      await fillIn(d.getByLabel("Timezone", { exact: true }), "Europe/Paris");
      await d.getByRole("button", { name: "Make it", exact: true }).click();
      await d.waitFor({ state: "detached" });
      await page
        .getByText(/Weekly on Sat, Sun at 02:00 for 4h \(Europe\/Paris\)/)
        .filter({ visible: true })
        .first()
        .waitFor();
      const rec = (await api("GET", "/api/observe/v1/downtimes")).body.downtimes.find((x) => x.monitor_identifier.monitor_id === id && x.schedule.recurrences);
      expect(rec && rec.schedule.recurrences[0].rrule === "FREQ=WEEKLY;BYDAY=SA,SU" && rec.schedule.timezone === "Europe/Paris", `the rule: ${JSON.stringify(rec)}`);
      ids.push(rec.id);
    } finally {
      for (const x of ids) await api("DELETE", `/api/observe/v1/downtimes/${x}`);
      await api("DELETE", `/api/observe/v1/monitors/${id}`);
    }
  },

  /** A ratio SLO made from the form over counts whose share failing is known: its standing as the
   * page and the API say it; its two burn-rate alerts added, then evaluated to the burn rate; its
   * deletion refused while they read it; its target changed; then deleted. */
  async "an SLO is made, alerted on, changed and deleted"() {
    const metric = "e2e.slo.requests";
    const tag = `run:${run}`;
    // Two hours of minutes ending at the last five-minute edge, 0.5% of each failing: an SLI of
    // 99.5% whatever window reads them, so half a 99% target's budget left and a burn rate of 0.5.
    const end = Math.floor(Date.now() / 300_000) * 300_000;
    const pts = (v) => Array.from({ length: 120 }, (_, i) => [(end - 120 * 60_000 + i * 60_000) / 1000, v]);
    const series = [
      { metric, type: "count", tags: ["status:ok", tag], points: pts(995) },
      { metric, type: "count", tags: ["status:error", tag], points: pts(5) },
    ];
    expect((await api("POST", "/api/observe/v1/series", { series })).status === 202, "writing the counts");
    const name = `E2E checkout ${run}`;
    const slo = (id) => api("GET", `/api/observe/v1/slos/${id}`);
    let id;
    const alerts = [];
    try {
      await page.goto(`${BASE}/observe/slos/new`);
      await fillIn(page.getByLabel("Name", { exact: true }), name);
      await fillIn(page.getByLabel("Good events", { exact: true }), `sum:${metric}{status:ok,${tag}}.as_count()`);
      await fillIn(page.getByLabel("All events", { exact: true }), `sum:${metric}{${tag}}.as_count()`);
      await fillIn(page.getByLabel("Target (%)", { exact: true }), "99");
      await fillIn(page.getByLabel("Warning (%, optional)", { exact: true }), "99.6");
      await page.getByRole("group", { name: "Timeframe", exact: true }).getByRole("button", { name: "7 days", exact: true }).click();
      await fillIn(page.getByLabel("Tags", { exact: true }), `e2e:${run}`);
      await page.getByRole("button", { name: "Make the SLO", exact: true }).click();
      await page.waitForURL(/\/observe\/slos\/s[0-9a-f]{16}$/);
      id = page.url().split("/").pop();
      // Every minute applied before it is read.
      const s = await until("its standing over all 120,000 events", async () => ((await slo(id)).body?.standing?.total === 120_000 ? (await slo(id)).body : null));
      expect(
        s.name === name && s.type === "ratio" && s.target === 99 && s.warning === 99.6 && s.timeframe === "7d" && JSON.stringify(s.tags) === JSON.stringify([`e2e:${run}`]),
        `kept as made: ${JSON.stringify(s)}`,
      );
      const st = s.standing;
      expect(
        st.state === "WARN" &&
          st.good === 119_400 &&
          Math.abs(st.sli - 99.5) < 1e-9 &&
          Math.abs(st.budget_left - 50) < 1e-6 &&
          ["1h", "6h", "1d", "3d"].every((w) => Math.abs(st.burn_rates[w] - 0.5) < 1e-9),
        `99.5% against 99%, warned under 99.6%: ${JSON.stringify(st)}`,
      );
      await page.reload();
      await page.locator("header .lead").getByText("Warn", { exact: true }).waitFor();
      const facts = page.locator("dl.facts");
      for (const t of ["99.500%", "99%, warn under 99.6%", "50.0%", "0.50× · 0.50× · 0.50× · 0.50×"]) await facts.getByText(t, { exact: true }).waitFor();
      await page.getByRole("region", { name: "Day by day" }).locator("canvas").first().waitFor();

      // The workbook's two alerts, for seven days: 2% of the budget in an hour, 5% in six.
      await page.getByRole("button", { name: "Add burn-rate alerts", exact: true }).click();
      for (const pace of ["fast", "steady"]) await page.getByRole("link", { name: `${name}: error budget burning ${pace}`, exact: true }).waitFor();
      alerts.push(...((await slo(id)).body.alerts ?? []).map((a) => a.id));
      const queries = await Promise.all(alerts.map(async (a) => (await api("GET", `/api/observe/v1/monitors/${a}`)).body.query));
      const want = [`slo("${id}").burn_rate("1h", "5m") > 3.36`, `slo("${id}").burn_rate("6h", "30m") > 1.4`];
      expect(JSON.stringify(queries.sort()) === JSON.stringify(want.sort()), `the alerts' queries: ${JSON.stringify(queries)}`);
      for (const a of alerts)
        await until(`${a} evaluated to the burn rate, 0.5, under its threshold`, async () => {
          const m = (await api("GET", `/api/observe/v1/monitors/${a}`)).body;
          return m.state === "OK" && m.groups?.length === 1 && Math.abs(m.groups[0].value - 0.5) < 1e-9;
        });

      // Refused while the alerts read it, and kept.
      await page.getByRole("button", { name: "Delete the SLO", exact: true }).click();
      await page.getByRole("dialog").getByRole("button", { name: "Delete", exact: true }).click();
      await page.getByText("Couldn't delete the SLO").first().waitFor();
      await page.getByRole("dialog").waitFor({ state: "detached" });
      problems.splice(0, problems.length, ...problems.filter((p) => !p.startsWith("HTTP 409 DELETE")));
      expect((await slo(id)).status === 200, "the SLO kept while its alerts read it");

      // In the list, warned.
      await page.goto(`${BASE}/observe/slos`);
      const row = page.getByRole("row").filter({ has: page.getByRole("link", { name, exact: true }) });
      await row.waitFor();
      const text = (await row.textContent()) ?? "";
      expect(/Warn/.test(text) && /99% · 7d/.test(text) && /99\.500%/.test(text), `its row: ${text}`);

      // Its target lowered under the SLI and its warning taken off: met.
      await row.getByRole("link", { name, exact: true }).click();
      await page.getByRole("button", { name: "Edit", exact: true }).click();
      await fillIn(page.getByLabel("Target (%)", { exact: true }), "99.4");
      await fillIn(page.getByLabel("Warning (%, optional)", { exact: true }), "");
      await page.getByRole("button", { name: "Save", exact: true }).click();
      await page.getByRole("button", { name: "Edit", exact: true }).waitFor();
      const changed = (await slo(id)).body;
      expect(changed.target === 99.4 && changed.warning === undefined && changed.standing.state === "OK" && changed.created === s.created, `changed: ${JSON.stringify(changed)}`);
      await page.locator("header .lead").getByText("OK", { exact: true }).waitFor();
      // 0.5% failing of a 0.6% budget: a sixth of it left.
      for (const t of ["99.4%", "16.7%"]) await page.locator("dl.facts").getByText(t, { exact: true }).waitFor();

      for (const a of alerts.splice(0)) expect((await api("DELETE", `/api/observe/v1/monitors/${a}`)).status < 300, `deleting ${a}`);
      await page.getByRole("button", { name: "Delete the SLO", exact: true }).click();
      await page.getByRole("dialog").getByRole("button", { name: "Delete", exact: true }).click();
      await page.waitForURL(/\/observe\/slos$/);
      expect((await slo(id)).status === 404, "the SLO gone");
      await page.getByRole("heading", { name: "SLOs", exact: true }).waitFor();
      // The list shows what it had until its refetch lands, as every list here does after a delete.
      await until("nor listed", async () => (await page.getByRole("link", { name, exact: true }).count()) === 0);
    } finally {
      for (const a of alerts) await api("DELETE", `/api/observe/v1/monitors/${a}`);
      if (id) await api("DELETE", `/api/observe/v1/slos/${id}`);
    }
  },

  /** A time-slice SLO made from the form: the minutes whose latency met its condition, of all. */
  async "a time-slice SLO counts the minutes that meet its condition"() {
    const metric = "e2e.slo.latency";
    const tag = `run:${run}`;
    // Two hours of minutes, the last twelve over 300 ms: 108 of the 120 slices met, 90%.
    const end = Math.floor(Date.now() / 300_000) * 300_000;
    const points = Array.from({ length: 120 }, (_, i) => [(end - 120 * 60_000 + i * 60_000) / 1000, i >= 108 ? 0.9 : 0.12]);
    expect((await api("POST", "/api/observe/v1/series", { series: [{ metric, type: "gauge", tags: [tag], points }] })).status === 202, "writing the latencies");
    const name = `E2E latency ${run}`;
    let id;
    try {
      await page.goto(`${BASE}/observe/slos/new`);
      await fillIn(page.getByLabel("Name", { exact: true }), name);
      await page.getByRole("group", { name: "Indicator", exact: true }).getByRole("button", { name: "Time slices", exact: true }).click();
      await fillIn(page.getByLabel("Query", { exact: true }), `max:${metric}{${tag}}`);
      await fillIn(page.getByLabel("Condition", { exact: true }), "< 0.3");
      await page.getByRole("group", { name: "Slice", exact: true }).getByRole("button", { name: "1 min", exact: true }).click();
      await fillIn(page.getByLabel("Target (%)", { exact: true }), "95");
      await page.getByRole("group", { name: "Timeframe", exact: true }).getByRole("button", { name: "7 days", exact: true }).click();
      await page.getByRole("button", { name: "Make the SLO", exact: true }).click();
      await page.waitForURL(/\/observe\/slos\/s[0-9a-f]{16}$/);
      id = page.url().split("/").pop();
      const s = await until("its 120 slices counted", async () => {
        const r = await api("GET", `/api/observe/v1/slos/${id}`);
        return r.body?.standing?.total === 120 ? r.body : null;
      });
      expect(s.type === "slices" && s.query === `max:${metric}{${tag}}` && s.condition === "< 0.3" && s.slice_s === 60, `kept as made: ${JSON.stringify(s)}`);
      // 10% of slices missed of a 5% budget: twice it spent, breached.
      const st = s.standing;
      expect(st.good === 108 && st.state === "BREACHED" && Math.abs(st.sli - 90) < 1e-9 && Math.abs(st.budget_left + 100) < 1e-6, `108 of 120 met: ${JSON.stringify(st)}`);
      await page.reload();
      await page.locator("header .lead").getByText("Breached", { exact: true }).waitFor();
      for (const t of ["90.0%", "95%", "-100.0%"]) await page.locator("dl.facts").getByText(t, { exact: true }).waitFor();
    } finally {
      if (id) await api("DELETE", `/api/observe/v1/slos/${id}`);
    }
  },
};

let failed = 0;
for (const [name, f] of Object.entries(scenarios).filter(([n]) => !process.env.ONLY || n.includes(process.env.ONLY))) {
  const t0 = Date.now();
  problems.length = 0;
  navs.length = 0;
  try {
    const note = await f();
    if (problems.length) throw new Error(problems.join("\n"));
    console.log(`ok   ${which}${phone ? " (phone)" : ""}  ${name}  ${Date.now() - t0} ms${note ? `  (${note})` : ""}`);
  } catch (e) {
    failed++;
    console.log(
      `FAIL ${which}${phone ? " (phone)" : ""}  ${name}\n     ${String(e.message ?? e)
        .split("\n")
        .join("\n     ")}`,
    );
    console.log(`     the page's loads and navigations:\n       ${navs.join("\n       ")}`);
    // What the page showed when it failed (SHOTS: a folder to keep it in).
    await page.screenshot({ path: `${process.env.SHOTS ?? "/tmp"}/observe-e2e-${which}${phone ? "-phone" : ""}-${Object.keys(scenarios).indexOf(name)}.png`, fullPage: true }).catch(() => undefined);
  }
}
await browser.close();
process.exit(failed ? 1 : 0);
