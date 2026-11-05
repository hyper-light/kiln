// One job of the browser lab: one browser on one client link, one target, one case, one round
// (docs/browser-lab.md; plan 01a1a832e9cc step 4). kiln-lab writes JOB.json and runs this in the
// client's namespace; it writes, beside JOB.json:
//   samples.jsonl   every operation the page measured (stamp or schedule, when parsed, size)
//   counters.jsonl  the client link's interface counters, every `window_ms` (its own namespace's
//                   /proc/self/net/dev: frames both ways, with every header, ACK and retransmission)
//   result.json     the calibration, the protocols each path came over, errors, and timings
import { readFileSync, writeFileSync, appendFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { get as httpsGet } from "node:https";
import { chromium, firefox, webkit } from "playwright";

const HERE = dirname(fileURLToPath(import.meta.url));
const BROWSERS = { chromium, webkit, firefox };
const PROBE_HTML = `<!doctype html><meta charset="utf-8"><title>kiln browser lab probe</title><script src="/__lab/probe.js"></script><p>probe</p>`;

function counters() {
  const text = readFileSync("/proc/self/net/dev", "utf8");
  const line = text.split("\n").find((l) => l.trim().startsWith("eth0:"));
  if (!line) return null;
  const v = line.split(":")[1].trim().split(/\s+/).map(Number);
  return { rx_bytes: v[0], rx_packets: v[1], tx_bytes: v[8], tx_packets: v[9] };
}

const nowUs = () => Math.round((performance.timeOrigin + performance.now()) * 1000);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

export async function runJob(path) {
  const job = JSON.parse(readFileSync(path, "utf8"));
  const dir = dirname(path);
  const out = (f) => join(dir, f);
  writeFileSync(out("samples.jsonl"), "");
  writeFileSync(out("counters.jsonl"), "");
  const result = { job: job.name, browser: job.browser, started_us: nowUs(), errors: [], protocols: {} };
  const type = BROWSERS[job.browser];
  if (!type) throw new Error(`no browser ${job.browser}`);
  // With a capture, the browser's TLS keys beside it (goal 7 reads the handshakes).
  const env = job.keylog ? { ...process.env, SSLKEYLOGFILE: job.keylog } : undefined;
  const browser = await type.launch({ args: job.browser === "chromium" ? job.chromium_args || [] : [], env });
  result.version = browser.version();
  const context = await browser.newContext({ ignoreHTTPSErrors: true });
  try {
    if (job.login) {
      const r = await context.request.post(job.origin + job.login.path, { data: { token: job.login.token } });
      if (!r.ok()) throw new Error(`login: ${r.status()}`);
    }
    const page = await context.newPage();
    page.on("pageerror", (e) => result.errors.push({ where: "page", error: String(e) }));
    // A stream the page closes at the end is aborted, which is no failure.
    page.on("requestfailed", (r) => r.failure()?.errorText !== "net::ERR_ABORTED" && result.errors.push({ where: "request", error: `${r.url()}: ${r.failure()?.errorText}` }));
    page.on("console", (m) => m.type() === "error" && result.errors.push({ where: "console", error: m.text() }));
    // The probe page, on the target's own origin; then no more routing, so nothing the page asks
    // for after it passes through Playwright.
    await page.route(`${job.origin}/__lab/**`, (route) => {
      const u = new URL(route.request().url());
      const headers = { "cross-origin-opener-policy": "same-origin", "cross-origin-embedder-policy": "require-corp", "cache-control": "no-store" };
      if (u.pathname === "/__lab/probe.js") return route.fulfill({ status: 200, headers: { ...headers, "content-type": "text/javascript" }, body: readFileSync(join(HERE, "probe/probe.js"), "utf8") });
      return route.fulfill({ status: 200, headers: { ...headers, "content-type": "text/html" }, body: PROBE_HTML });
    });
    await page.goto(`${job.origin}/__lab/probe.html`);
    await page.waitForFunction(() => !!window.__lab);
    await page.unrouteAll({ behavior: "wait" });
    result.isolated = await page.evaluate(() => window.crossOriginIsolated);
    // The page's timer grain: the smallest step performance.now() takes, over 200,000 reads.
    result.timer_grain_us = await page.evaluate(() => {
      let last = performance.now();
      let grain = Infinity;
      for (let i = 0; i < 200000; i++) {
        const t = performance.now();
        if (t > last) grain = Math.min(grain, t - last);
        last = t;
      }
      return grain * 1000;
    });
    // One request to the origin, so the page says which protocol its operations came over (a
    // socket has no timing entry of its own).
    if (job.protocol_path) await page.evaluate((p) => fetch(p, { cache: "no-store" }).then((r) => r.arrayBuffer()), job.protocol_path).catch((e) => result.errors.push({ where: "protocol", error: String(e) }));
    // The control path checked from the driver itself first, so a failed calibration says why.
    if (job.control) {
      const u = new URL(job.control.replace(/^wss:/, "https:"));
      result.control_check = await new Promise((ok) => {
        const req = httpsGet({ host: u.hostname, port: u.port, path: "/", rejectUnauthorized: false, timeout: 5000 }, (res) => {
          res.resume();
          ok(`driver: ${res.statusCode}`);
        });
        req.on("error", (e) => ok(`driver: ${e.message}`));
        req.on("timeout", () => req.destroy(new Error("timed out")));
      });
      result.control_page = await page.evaluate((u) => fetch(u, { mode: "cors" }).then((r) => `page fetch: ${r.status}`, (e) => `page fetch: ${e.message}`), u.href);
    }
    if (job.control) result.calibration = await page.evaluate(([u, n]) => window.__lab.calibrate(u, n, 2 * n), [job.control, job.calibrate_n || 200]);
    // The link quiet before the job starts.
    await sleep(job.settle_ms ?? 1000);
    const start = nowUs();
    result.window_start_us = start;
    const series = setInterval(() => appendFileSync(out("counters.jsonl"), JSON.stringify({ t_us: nowUs(), ...counters() }) + "\n"), job.window_ms || 1000);
    appendFileSync(out("counters.jsonl"), JSON.stringify({ t_us: nowUs(), ...counters() }) + "\n");
    for (const op of job.ops) {
      if (op.case === "request") await page.evaluate((o) => window.__lab.requests(o), op);
      else await page.evaluate((o) => window.__lab.stream(o), op);
    }
    // Ready: kiln-lab starts the server's side (a turn, a replay) once this file is there.
    writeFileSync(out("ready"), String(nowUs()));
    const until = Date.now() + job.duration_s * 1000;
    const drain = async () => {
      const d = await page.evaluate(() => window.__lab.drain());
      if (d.samples.length) appendFileSync(out("samples.jsonl"), d.samples.map((s) => JSON.stringify(s)).join("\n") + "\n");
      result.errors.push(...d.errors);
      Object.assign(result.protocols, d.protocols);
    };
    while (Date.now() < until) {
      await sleep(1000);
      await drain();
    }
    // What was still on its way gets `tail_s` to arrive.
    await sleep((job.tail_s ?? 0) * 1000);
    await drain();
    clearInterval(series);
    appendFileSync(out("counters.jsonl"), JSON.stringify({ t_us: nowUs(), ...counters() }) + "\n");
    result.window_end_us = nowUs();
    await page.evaluate(() => window.__lab.close());
    // The requests closing found unanswered, as misses.
    await drain();
    result.page_protocol = await page.evaluate(() => performance.getEntriesByType("navigation")[0]?.nextHopProtocol || "");
  } catch (e) {
    result.errors.push({ where: "driver", error: String(e && e.stack ? e.stack : e) });
    result.failed = true;
  } finally {
    await context.close().catch(() => {});
    await browser.close().catch(() => {});
    writeFileSync(out("result.json"), JSON.stringify(result, null, 1));
  }
}
