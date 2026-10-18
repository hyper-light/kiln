// Observe's UI, as a person uses it, against a live kiln server (docs/observe.md §13): each
// scenario drives the pages, then checks what they did through the API (and, for integrations,
// what a receiver was sent). It changes what it makes and removes it after.
//
//   KILN_URL=http://127.0.0.1:17170 KILN_TOKEN=$(cat <data>/admin.token) \
//   PLAYWRIGHT_CORE=/path/to/node_modules/playwright-core/index.mjs \
//   [BROWSER=chromium|webkit] [PHONE=1] [ONLY=<part of a scenario's name>] [SHOTS=<folder>] \
//   [RECEIVER=http://127.0.0.1:17199 RECEIVED=<the jsonl it keeps what it is sent in>] \
//   node ui/e2e/observe.mjs
//
// The server needs observe's data to look at: metrics of `app.latency.p95` by `service` and logs
// with `status:error` among them (what the seed of docs/observe.md §13 writes). playwright-core is
// not a dependency of the UI's: point PLAYWRIGHT_CORE at one, of the browsers installed.
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
page.on("console", (m) => m.type() === "error" && !m.text().startsWith("Failed to load resource") && problems.push(`console: ${m.text()}`));
page.on("response", (r) => r.status() >= 400 && problems.push(`HTTP ${r.status()} ${r.request().method()} ${r.url().replace(BASE, "")}`));
if (process.env.TRACE) page.on("request", (r) => r.url().includes("/api/") && console.log(`  ${Date.now() % 100000} ${r.method()} ${r.url().replace(BASE, "")}`));

const scenarios = {
  /** A query explored, put on a new dashboard, the dashboard given a note and saved. */
  async "a query goes from the explorer onto a new dashboard, which is changed and saved"() {
    const query = "avg:app.latency.p95{*} by {service}";
    await page.goto(`${BASE}/observe/metrics`);
    await page.locator("#observe-query").fill(query);
    await page.getByRole("button", { name: "Run" }).click();
    await page.waitForURL(/\/observe\/metrics\?q=/);
    const rows = page.locator("table.data-table tbody tr");
    await until("the series' rows", async () => (await rows.count()) === 3);
    const labels = (await rows.locator("td:first-child").allTextContents()).map((t) => t.trim()).sort();
    expect(JSON.stringify(labels) === JSON.stringify(["service:api", "service:web", "service:worker"]), `the series by service, got ${labels}`);
    await page.getByRole("button", { name: "Add to dashboard" }).click();
    const dialog = page.getByRole("dialog");
    await dialog.getByLabel("Dashboard", { exact: true }).selectOption("new");
    await dialog.getByLabel("New dashboard's title").fill(`E2E ${run}`);
    await dialog.getByLabel("Widget's title").fill("Latency by service");
    await dialog.getByRole("button", { name: "Add" }).click();
    await page.waitForURL(/\/observe\/dashboards\/d[0-9a-f]+/);
    const id = decodeURIComponent(page.url().split("/").pop().split("?")[0]);
    await page.getByRole("heading", { name: `E2E ${run}` }).waitFor();
    await page.getByRole("heading", { name: "Latency by service" }).waitFor();
    let d = await api("GET", `/api/observe/v1/dashboards/${id}`);
    expect(d.status === 200 && d.body.widgets.length === 1 && d.body.widgets[0].queries[0] === query, "the dashboard kept, its widget of the query");
    // A note added in place, then saved.
    await page.getByRole("button", { name: "Edit" }).click();
    await page.getByRole("button", { name: "Add a widget" }).click();
    await page.getByRole("dialog").getByLabel("Kind").selectOption("note");
    await page.getByRole("dialog").getByLabel("Title").fill("Read me");
    await page.locator("#widget-text").fill("Latency is **p95**, by service.");
    await page.getByRole("dialog").getByRole("button", { name: "Add" }).click();
    await page.getByRole("button", { name: "Save" }).click();
    await page.getByRole("button", { name: "Edit" }).waitFor();
    await page.locator(".obs-note strong", { hasText: "p95" }).waitFor();
    d = await api("GET", `/api/observe/v1/dashboards/${id}`);
    expect(d.body.widgets.length === 2 && d.body.widgets[1].kind === "note" && d.body.widgets[1].layout.y >= d.body.widgets[0].layout.y, "two widgets, the note after the chart");
    // Deleted from its page: gone from the list.
    await page.getByRole("button", { name: "Delete the dashboard" }).click();
    await page.getByRole("dialog").getByRole("button", { name: "Delete" }).click();
    await page.waitForURL(/\/observe\/dashboards$/);
    expect((await api("GET", `/api/observe/v1/dashboards/${id}`)).status === 404, "the dashboard gone");
  },

  /** kiln's own dashboard narrowed to one host by its template variable. */
  async "a template variable narrows every widget to its value"() {
    const hosts = (await api("GET", "/api/observe/v1/tags?key=host")).body.values;
    expect(hosts.length > 0, "a host to choose");
    await page.goto(`${BASE}/observe/dashboards/builtin:hosts`);
    await page.getByLabel("$host").selectOption(hosts[0]);
    await page.waitForURL(new RegExp(`var\\.host=${encodeURIComponent(hosts[0])}`));
    // A copy changes; kiln's own does not.
    expect((await page.getByRole("button", { name: "Edit" }).count()) === 0, "kiln's own dashboard has no Edit");
    await page.getByRole("button", { name: "Copy to change" }).click();
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
    await page.goto(`${BASE}/observe/logs`);
    await page.locator("#observe-search").fill("status:error");
    await page.getByRole("button", { name: "Search" }).click();
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
    expect(
      viaApi.status === 200 && viaApi.body.records.length > 0 && viaApi.body.records.every((r) => r.status === "error" && r.service === name && r.attributes.route === value),
      "the API agrees with the page's search",
    );
  },

  /** A monitor made from the form, muted, unmuted and deleted. */
  async "a monitor is made, muted, unmuted and deleted"() {
    await page.goto(`${BASE}/observe/monitors/new?query=${encodeURIComponent("avg(last_5m):avg:app.latency.p95{*} by {service} > 1000000")}`);
    await page.getByLabel("Name", { exact: true }).fill(`E2E latency ${run}`);
    await page.getByLabel("Warning at", { exact: true }).fill("900000");
    await page.getByRole("button", { name: "Create" }).click();
    await page.waitForURL(/\/observe\/monitors\/m[0-9a-f]+$/);
    const id = page.url().split("/").pop();
    let m = await api("GET", `/api/observe/v1/monitors/${id}`);
    expect(m.status === 200 && m.body.name === `E2E latency ${run}` && m.body.options.thresholds.warning === 900000, "the monitor kept as made");
    await page.getByRole("button", { name: "Mute" }).click();
    await page.getByText("· muted").first().waitFor();
    m = await api("GET", `/api/observe/v1/monitors/${id}`);
    expect(m.body.muted_until > Date.now(), "muted on the server");
    await page.getByRole("button", { name: "Unmute" }).click();
    await page.getByRole("button", { name: "Mute" }).waitFor();
    m = await api("GET", `/api/observe/v1/monitors/${id}`);
    expect(!(m.body.muted_until > Date.now()), "unmuted on the server");
    await page.getByRole("button", { name: "Delete the monitor" }).click();
    await page.getByRole("dialog").getByRole("button", { name: "Delete" }).click();
    await page.waitForURL(/\/observe\/monitors$/);
    expect((await api("GET", `/api/observe/v1/monitors/${id}`)).status === 404, "the monitor gone");
  },

  /** A signed webhook added, tested (the receiver gets a signed notice), and removed. */
  async "an integration is added, tested and removed"() {
    const receiver = process.env.RECEIVER;
    if (!receiver) return "skipped: no RECEIVER";
    const name = `e2e-${run}`;
    await page.goto(`${BASE}/observe/integrations`);
    await page.getByRole("button", { name: "Add integration" }).click();
    const dialog = page.getByRole("dialog");
    await dialog.getByLabel("Kind").selectOption("webhook");
    await dialog.getByLabel("Name").fill(name);
    await dialog.getByLabel("URL").fill(`${receiver}/${name}`);
    await dialog.getByLabel("Signing secret (optional)").fill("e2e-secret");
    await dialog.getByRole("button", { name: "Add" }).click();
    const row = page.locator("tr", { hasText: name });
    await row.waitFor();
    const before = fs.existsSync(process.env.RECEIVED) ? fs.readFileSync(process.env.RECEIVED, "utf8").split("\n").filter(Boolean).length : 0;
    await row.getByRole("button", { name: "Test" }).click();
    const got = await until("the test notice at the receiver", () => {
      const lines = fs.existsSync(process.env.RECEIVED) ? fs.readFileSync(process.env.RECEIVED, "utf8").split("\n").filter(Boolean) : [];
      return lines
        .slice(before)
        .map((l) => JSON.parse(l))
        .find((x) => x.path === `/${name}`);
    });
    expect(/^sha256=[0-9a-f]{64}$/.test(got.sig ?? ""), `a signed notice, got ${got.sig}`);
    await row.getByRole("button", { name: `Remove ${name}` }).click();
    await page.getByRole("dialog").getByRole("button", { name: "Remove" }).click();
    await until("the integration gone", async () => !(await api("GET", "/api/observe/v1/integrations")).body.integrations.some((i) => i.name === name));
  },
  /** A search's patterns: the seed's message shapes grouped, and a pattern a click from its records. */
  async "a search's records group into patterns, and a pattern narrows the search to its own"() {
    await page.goto(`${BASE}/observe/logs?view=patterns&range=4h&q=${encodeURIComponent("source:seed")}`);
    const rows = page.getByRole("region", { name: "Patterns" }).getByRole("listitem");
    await until("the patterns", async () => (await rows.count()) >= 5, 20_000);
    const refused = rows.filter({ hasText: "connection refused by db-" });
    await refused.first().waitFor();
    await refused.first().getByRole("button").click();
    await page.waitForURL((u) => !u.searchParams.has("view") && (u.searchParams.get("q") ?? "").includes('"refused"'));
    const q = new URL(page.url()).searchParams.get("q");
    await page.getByRole("region", { name: "Records" }).waitFor();
    const to = Date.now();
    const found = await api("POST", "/api/observe/v1/logs/search", { query: `-source:kiln.event (${q})`, from: to - 4 * 3_600_000, to, limit: 200 });
    expect(found.status === 200 && found.body.records.length > 0, `the narrowed search finds records: ${JSON.stringify(found.body).slice(0, 200)}`);
    expect(
      found.body.records.every((r) => /^connection refused by db-\d+:\d+$/.test(r.message)),
      `every record the pattern narrows to is of it: ${found.body.records.map((r) => r.message).slice(0, 5)}`,
    );
  },

  /**
   * A cloud added from the form (a key that is not one refused; the server's own credentials tested,
   * the test saying why they are not there), and one the API made against a stand-in for CloudWatch
   * shown pulling, edited without its secret (kept: the next pull is signed by its key), pulled now,
   * and both removed.
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
      await page.getByRole("button", { name: "Add cloud" }).click();
      const add = page.getByRole("dialog");
      await add.getByLabel("Cloud", { exact: true }).selectOption("gcp");
      await add.getByLabel("Name", { exact: true }).fill(`${name}-g`);
      await add.getByLabel("Service account key (JSON)").fill("not a key");
      await add.getByText("Not a key file's JSON.").waitFor();
      expect(await add.getByRole("button", { name: "Add" }).isDisabled(), "a key that is not one: Add is off");
      await add.getByLabel("Credentials", { exact: true }).selectOption("server");
      await add.getByRole("button", { name: "Test" }).click();
      await add
        .getByRole("status")
        .getByText(/no Google credentials/)
        .waitFor({ timeout: 20_000 });
      await add.getByRole("button", { name: "Add" }).click();
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
        interval_s: 300,
      });
      expect(made.status === 204, `the cloud made, got ${made.status}`);
      const row = rowOf(name);
      await until("shown pulling, its series counted", async () => /Pulling/.test((await row.textContent()) ?? "") && /3 series/.test((await row.textContent()) ?? ""), 30_000);
      await row.getByRole("button", { name: `Edit ${name}` }).click();
      const edit = page.getByRole("dialog");
      await edit.getByText("Kept. Type a new one to replace it.").waitFor();
      await edit.getByLabel("Every (seconds)").fill("120");
      await edit.getByRole("button", { name: "Save" }).click();
      const after = await until("saved", async () => (await clouds()).find((c) => c.name === name && c.interval_s === 120));
      expect(after.secret_access_key === "…" && after.endpoint === endpoint && after.access_key_id === "AKIDE2E", `its secret, key and endpoint kept: ${JSON.stringify(after)}`);
      const calls = seen.length;
      await row.getByRole("button", { name: `Pull ${name} now` }).click();
      await until("a pull after the change, signed by its key", async () => seen.length > calls && seen.slice(calls).every((s) => s.auth.includes("Credential=AKIDE2E/")));
      await until("pulling still", async () => (await clouds()).find((c) => c.name === name)?.status.state === "ok");
      for (const n of [name, `${name}-g`]) {
        await rowOf(n)
          .getByRole("button", { name: `Remove ${n}` })
          .click();
        await page.getByRole("dialog").getByRole("button", { name: "Remove" }).click();
        await until(`${n} gone`, async () => !(await clouds()).some((c) => c.name === n));
      }
    } finally {
      cw.close();
      for (const n of [name, `${name}-g`]) await api("DELETE", `/api/observe/v1/clouds/${n}`);
    }
  },
};

let failed = 0;
for (const [name, f] of Object.entries(scenarios).filter(([n]) => !process.env.ONLY || n.includes(process.env.ONLY))) {
  const t0 = Date.now();
  problems.length = 0;
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
    // What the page showed when it failed (SHOTS: a folder to keep it in).
    await page.screenshot({ path: `${process.env.SHOTS ?? "/tmp"}/observe-e2e-${which}${phone ? "-phone" : ""}-${Object.keys(scenarios).indexOf(name)}.png`, fullPage: true }).catch(() => undefined);
  }
}
await browser.close();
process.exit(failed ? 1 : 0);
