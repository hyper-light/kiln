#!/usr/bin/env node
/**
 * The page-load lab's driver (goal 8, plan 01a1a832ebfd step 1): loads goal 8's pages in Chromium or
 * WebKit, desktop or phone, as a first visit (empty cache), a repeat visit (same build, the cache
 * kept, a new tab), a visit after a deploy (the cache from build A, the server now on build B), or
 * an in-app move to a second page, and records per run: requests, bytes by resource type, duplicate
 * downloads (one URL twice, or one content under two URLs), and the times to first paint, readable
 * (the page's primary surface shows content), ready (it takes input) and settled (the network quiet
 * for QUIET_MS). No DevTools throttling: the link is goal 1's (netem in its network namespace, with
 * its byte counters, which `--link-bytes` reads before and after each run); without it, the bytes
 * here are what the browser says it received (Chromium: CDP's encodedDataLength, headers included;
 * WebKit: Resource Timing's transferSize).
 *
 *   node ci/ui/pageload.mjs --base https://kiln.lab:8443 --engine chromium|webkit --form desktop|phone
 *       --visit first|repeat|deploy|move --page conversation [--runs 10] [--link-bytes "cmd"]
 *       [--deploy "cmd that moves the server to build B"] [--out file.jsonl]
 *
 * A repeat visit whose hashed assets came over the network again (the cache not in use: an
 * untrusted certificate, say) is refused, not reported as warm: Chromium keeps nothing it fetched
 * over a certificate error, so the lab's root must be one the browser trusts.
 */
import { execSync } from "node:child_process";
import { createHash } from "node:crypto";
import { appendFileSync, mkdtempSync, rmSync } from "node:fs";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const QUIET_MS = 2000;

/** Goal 8's pages: where each is, what shows its primary surface has content, and what takes input. */
export const PAGES = {
  signin: { path: "/", readable: "form input", ready: "form input:not([disabled])", signedOut: true },
  overview: { path: "/", readable: "main h1, main h2, main [role=heading]", ready: "main a, main button" },
  mission: { path: "/agents", readable: "main [data-row], main .ms-row, main li", ready: "textarea, [contenteditable=true], main button" },
  conversation: { path: "/agents/{agent}", readable: "main article, main [data-part], main p", ready: "textarea:not([disabled]), [contenteditable=true]" },
  runs: { path: "/runs", readable: "main tr, main li, main [data-row]", ready: "main a, main button" },
  run: { path: "/runs/{run}", readable: "main pre, main [data-log], main li", ready: "main a, main button" },
};

const kindOf = (url, type) => {
  const p = new URL(url).pathname;
  if (p.startsWith("/api/")) return "data";
  if (/\.js$/.test(p)) return "js";
  if (/\.css$/.test(p)) return "css";
  if (/\.woff2?$/.test(p)) return "font";
  if (/\.wasm$/.test(p)) return "wasm";
  if (type === "document" || !path.extname(p)) return "html";
  return "other";
};

async function launch(engine, form, dir) {
  // The UI's own Playwright (ui/node_modules).
  const pw = createRequire(path.join(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "ui", "package.json"))("@playwright/test");
  const type = engine === "webkit" ? pw.webkit : pw.chromium;
  const device = form === "phone" ? (engine === "webkit" ? pw.devices["iPhone 15"] : pw.devices["Pixel 7"]) : { viewport: { width: 1440, height: 900 } };
  // A persistent profile: the cache a repeat visit reads survives between contexts of a run.
  const ctx = await type.launchPersistentContext(dir, { ...device, ignoreHTTPSErrors: false, channel: engine === "chromium" ? process.env.E2E_CHANNEL : undefined });
  return ctx;
}

/**
 * One visit: a new page (tab) in `ctx` loading `url`, or (an in-app move) `page` already loaded,
 * following its link to `url`; watched until the network is quiet.
 */
async function visit(ctx, url, def, engine, loaded = null) {
  const page = loaded ?? (await ctx.newPage());
  const reqs = [];
  const byId = new Map();
  let cdp;
  if (engine === "chromium") {
    cdp = await ctx.newCDPSession(page);
    await cdp.send("Network.enable");
    cdp.on("Network.responseReceived", (e) =>
      byId.set(e.requestId, { url: e.response.url, type: e.type?.toLowerCase(), status: e.response.status, cached: e.response.fromDiskCache || e.response.fromServiceWorker, bytes: 0 }),
    );
    cdp.on("Network.requestServedFromCache", (e) => byId.set(e.requestId, { ...(byId.get(e.requestId) ?? {}), cached: true }));
    cdp.on("Network.loadingFinished", (e) => {
      const r = byId.get(e.requestId);
      if (r) reqs.push({ ...r, bytes: r.cached ? 0 : e.encodedDataLength });
    });
  }
  let last = Date.now();
  page.on("request", () => (last = Date.now()));
  page.on("requestfinished", () => (last = Date.now()));
  const t0 = Date.now();
  if (loaded)
    await page
      .locator(`a[href="${new URL(url).pathname}"]`)
      .first()
      .click({ timeout: 60_000 });
  else await page.goto(url, { waitUntil: "commit" });
  const at = async (sel) => {
    try {
      await page.waitForSelector(sel, { timeout: 600_000, state: "visible" });
      return Date.now() - t0;
    } catch {
      return null;
    }
  };
  const readable = await at(def.readable);
  const ready = await at(def.ready);
  while (Date.now() - last < QUIET_MS) await page.waitForTimeout(250);
  const settled = last - t0;
  const timing = await page.evaluate(
    (since) => ({
      paint: performance.getEntriesByType("paint").find((e) => e.name === "first-contentful-paint")?.startTime ?? performance.getEntriesByType("paint")[0]?.startTime ?? null,
      resources: performance
        .getEntriesByType("resource")
        .filter((r) => r.startTime >= since)
        .map((r) => ({ url: r.name, bytes: r.transferSize, body: r.encodedBodySize, type: r.initiatorType, start: r.startTime })),
      nav: (() => {
        const n = performance.getEntriesByType("navigation")[0];
        return n && since === 0 ? { url: n.name, bytes: n.transferSize, body: n.encodedBodySize } : null;
      })(),
    }),
    loaded ? (await page.evaluate(() => performance.now())) - (Date.now() - t0) : 0,
  );
  const list = engine === "chromium" ? reqs : [...(timing.nav ? [{ ...timing.nav, type: "document" }] : []), ...timing.resources];
  const bodies = new Map();
  // The same content under two URLs: the page's own files (not its API data, goal 9's), 1 KiB and up.
  if (engine === "chromium")
    for (const r of list.filter((x) => !x.cached && x.bytes > 1024 && kindOf(x.url, x.type) !== "data")) {
      try {
        const res = await page.request.get(r.url, { headers: { "accept-encoding": "identity" } });
        bodies.set(
          r.url,
          createHash("sha256")
            .update(await res.body())
            .digest("hex"),
        );
      } catch {
        /* a body we can't read again doesn't count for duplicates */
      }
    }
  await page.close();
  const by = {};
  for (const r of list) {
    const k = kindOf(r.url, r.type);
    by[k] = (by[k] ?? 0) + (r.bytes ?? 0);
  }
  const seen = new Map();
  const twice = [];
  for (const r of list.filter((x) => (x.bytes ?? 0) > 0)) {
    seen.set(r.url, (seen.get(r.url) ?? 0) + 1);
    if (seen.get(r.url) === 2) twice.push(new URL(r.url).pathname);
  }
  const byBody = new Map();
  for (const [u, h] of bodies) byBody.set(h, [...(byBody.get(h) ?? []), u]);
  const sameContent = [...byBody.values()].filter((us) => new Set(us).size > 1).map((us) => [...new Set(us)].map((u) => new URL(u).pathname).join(" = "));
  // A file of the page's downloaded twice counts; its API data asked twice is said apart (goal 9's to cut).
  const dataTwice = twice.filter((p) => p.startsWith("/api/"));
  const dupUrl = twice.length - dataTwice.length;
  const dupContent = sameContent.length;
  // The heavy parts (the editor, xterm, uPlot, tree-sitter and its grammars) asked before the page was ready (step 6: none, unless its first view shows them).
  const HEAVY = /xterm|codemirror|editor|uplot|tree-sitter|\.wasm/i;
  const heavyBeforeReady = ready === null ? null : timing.resources.filter((r) => HEAVY.test(r.url) && r.start < ready).reduce((a, r) => a + (r.bytes || r.body || 0), 0);
  const assetsDown = list.filter((r) => /\/assets\//.test(r.url) && (r.bytes ?? 0) > 300).reduce((a, r) => a + r.bytes, 0);
  return {
    requests: list.length,
    bytes: by,
    assets_down: assetsDown,
    duplicates: dupUrl + dupContent,
    twice: twice.filter((p) => !p.startsWith("/api/")),
    same_content: sameContent,
    data_twice: dataTwice,
    heavy_before_ready: heavyBeforeReady,
    paint: timing.paint,
    readable,
    ready,
    settled,
  };
}

function linkBytes(cmd) {
  if (!cmd) return null;
  const [down, up] = execSync(cmd, { encoding: "utf8" }).trim().split(/\s+/).map(Number);
  return { down, up };
}

async function main(argv) {
  const flag = (k, d) => {
    const i = argv.indexOf(k);
    return i < 0 ? d : argv[i + 1];
  };
  const base = flag("--base", "http://127.0.0.1:17073");
  const engine = flag("--engine", "chromium");
  const form = flag("--form", "desktop");
  const kind = flag("--visit", "first");
  const name = flag("--page", "conversation");
  const runs = Number(flag("--runs", "1"));
  const out = flag("--out", null);
  const def = PAGES[name];
  if (!def) throw new Error(`no page ${name}`);
  const url = base + def.path.replace("{agent}", flag("--agent", "long")).replace("{run}", flag("--run", "1"));
  for (let i = 0; i < runs; i++) {
    const dir = mkdtempSync(path.join(process.env.TMPDIR ?? tmpdir(), "pageload-"));
    const ctx = await launch(engine, form, dir);
    try {
      // What the visit measured comes after what it starts from: a first visit from nothing, a repeat
      // from the same build's load, a deploy from build A's, a move from the first page's.
      if (kind === "repeat" || kind === "deploy") await visit(ctx, url, def, engine);
      if (kind === "deploy") execSync(flag("--deploy", "false"), { stdio: "inherit" });
      const before = linkBytes(flag("--link-bytes", null));
      const r = kind === "move" ? await move(ctx, base, url, def, engine, flag("--to", "mission")) : await visit(ctx, url, def, engine);
      const after = linkBytes(flag("--link-bytes", null));
      const link = before && after ? { down: after.down - before.down, up: after.up - before.up } : null;
      if (kind === "repeat" && r.assets_down > 0) {
        console.error(`pageload: refused: the repeat visit downloaded ${r.assets_down} bytes of hashed assets again (the cache isn't in use: is the lab's root trusted?)`);
        process.exitCode = 2;
        continue;
      }
      const row = { page: name, visit: kind, engine, form, run: i, link, ...r };
      console.log(JSON.stringify(row));
      if (out) appendFileSync(out, `${JSON.stringify(row)}\n`);
    } finally {
      await ctx.close();
      rmSync(dir, { recursive: true, force: true });
    }
  }
}

/** An in-app move: the first page loaded and quiet, then its link to the second followed in the same tab. */
async function move(ctx, base, url, def, engine, to) {
  const page = await ctx.newPage();
  await page.goto(url, { waitUntil: "networkidle" });
  await page.waitForSelector(def.ready, { timeout: 600_000 });
  const next = PAGES[to];
  return visit(ctx, base + next.path, next, engine, page);
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url))
  main(process.argv.slice(2)).catch((e) => {
    console.error(e);
    process.exitCode = 1;
  });
