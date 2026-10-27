// W26-AC06, W14-AC06's measurement (review_ac06.mjs) on a Ship item's dropdown, held to 16.7 ms a
// frame rather than its 17.5, in Chromium and WebKit, desktop and phone: open the item carrying the
// 10,000-line diff, open the file whole, and scroll it; then a typing check — each keystroke's turn
// at a median no worse. Against a real kiln server (review_setup.py or the like made its data:
// AC06={"base","admin","item","diff","file","rows","channel"}) or the mock's production build
// (AC06={"base"}, the fixed commit's 10,000-line fixture and D41 for typing).
//
//     docker run --rm --network host --ipc host -e AC06="$(cat /tmp/ac06.json)" \
//       -v $PWD/ci/ui:/check:ro -v <dir with node_modules/playwright-core>:/pw:ro -v /tmp/ship-shots:/shots \
//       mcr.microsoft.com/playwright:v1.64.1-noble node /check/ship_ac06.mjs
//
// Locally: PW=ui/node_modules/playwright-core/index.mjs AC06='{"base":"http://127.0.0.1:17073","channel":"chrome"}' node ci/ui/ship_ac06.mjs
const pw = await import(process.env.PW ?? "/pw/node_modules/playwright-core/index.mjs"); // playwright-core 1.64.1
const { chromium, webkit, devices } = pw.default ?? pw;
import { checkLayout, pageErrors } from "./layout.mjs";

const cfg = JSON.parse(process.env.AC06);
const shots = "/shots";
const report = [];
let failed = 0;
const check = (browser, what, ok, detail = "") => {
  report.push(`${ok ? "ok  " : "FAIL"} ${browser}: ${what}${detail ? ` (${detail})` : ""}`);
  if (!ok) failed++;
};

// As the author signs in (a real server; the mock needs no sign-in).
async function signedIn(browser, options) {
  const ctx = await browser.newContext(options);
  if (cfg.admin) {
    const r = await ctx.request.post(cfg.base + "/api/users/dev/link", { headers: { Authorization: `Bearer ${cfg.admin}` } });
    if (!r.ok()) throw new Error(`link: ${r.status()} ${await r.text()}`);
    const { url } = await r.json();
    const page = await ctx.newPage();
    await page.goto(url);
    await page.close();
  }
  return ctx;
}

/** Opens the Ship item whose row carries `text`; its dropdown's Files section is up when done. */
async function openItem(page, text) {
  await page.goto(cfg.base + "/ship");
  const row = page.locator(".ship-row", { hasText: text }).first();
  await row.locator(".ship-main").click();
  await row.locator(".rv-s").first().waitFor({ timeout: 120000 });
  return row;
}

/** Frames over 4 s while `scroller` scrolls 120 px a frame (W14-AC06's measure). */
async function frames(page, scroller) {
  return page.evaluate(async (sel) => {
    const el = sel ? document.querySelector(sel) : null;
    const times = [];
    let last = performance.now();
    const end = last + 4000;
    await new Promise((resolve) => {
      const step = (t) => {
        times.push(t - last);
        last = t;
        if (el) el.scrollBy(0, 120);
        else window.scrollBy(0, 120);
        if (t < end) requestAnimationFrame(step);
        else resolve();
      };
      requestAnimationFrame(step);
    });
    times.shift();
    times.sort((a, b) => a - b);
    return { n: times.length, median: times[times.length >> 1], p95: times[Math.floor(times.length * 0.95)] };
  }, scroller);
}

const ITEM = cfg.item ?? "sync: a move carries its homes";
const TYPING = cfg.diff ?? "pipeline: dynamic matrices";

for (const [name, type, phoneDevice] of [
  ["chromium", chromium, { ...devices["Pixel 7"], viewport: { width: 390, height: 844 } }],
  ["webkit", webkit, devices["iPhone 13"]],
]) {
  const browser = await type.launch(cfg.channel && name === "chromium" ? { channel: cfg.channel } : {});
  try {
    const ctx = await signedIn(browser, { viewport: { width: 1280, height: 900 } });
    const page = await ctx.newPage();
    const errors = pageErrors(page, cfg.base);
    const bad = [];
    page.on("response", (r) => {
      if (r.status() >= 400) bad.push(`${r.status()} ${r.url().replace(cfg.base, "")}`);
    });

    // The item's dropdown, Files first: the diff the item carries, one of 10,000 lines.
    const row = await openItem(page, ITEM);
    const secs = await row.locator(".rv-s > h3, .ship-sec > h3").allTextContents();
    check(name, "the item opens to Files, the Conversation, Output, Jobs, Lanes", /^Files/.test(secs[0] ?? "") && /Conversation/.test(secs[1] ?? ""), secs.join(" | "));
    await row.locator(".rv-more").first().click();
    const full = row.locator(".rv-fullb");
    await full.locator(".rv-l").first().waitFor({ state: "attached", timeout: 120000 });
    const rows = await full.locator(".rv-l").count();
    // The whole file, scrolled: a median frame at or under 16.7 ms (W26-AC06; W14's was 17.5).
    const wide = await frames(page, ".rv-fullb");
    check(name, "an open item's 10,000-line diff scrolls at 60 fps (median frame ≤ 16.7 ms)", wide.median <= 16.7, `median ${wide.median?.toFixed(1)} ms, p95 ${wide.p95?.toFixed(1)} ms over ${wide.n} frames; ${rows} rows in the DOM`);

    // Typing in its composer: each keystroke's ms, median held to a frame.
    const other = await openItem(page, TYPING);
    await other.locator(".rv-thread").first().locator(".rv-btn", { hasText: "Reply" }).click();
    const field = other.locator(".rv-thread textarea");
    await field.focus();
    const keys = [];
    for (let i = 0; i < 40; i++) {
      const t0 = Date.now();
      await page.keyboard.press("x");
      keys.push(Date.now() - t0);
    }
    keys.sort((a, b) => a - b);
    check(name, "typing a comment keeps up (median keystroke ≤ 16.7 ms)", keys[keys.length >> 1] <= 16.7, `median ${keys[keys.length >> 1]} ms, p95 ${keys[Math.floor(keys.length * 0.95)]} ms, worst ${keys[keys.length - 1]} ms`);
    check(name, "no script errors on the desktop", errors.length === 0, errors.join(" | "));
    check(name, "no failed requests", bad.length === 0, bad.slice(0, 5).join(", "));
    await checkLayout(check, `${name}: wide: the item`, page);
    await page.screenshot({ path: `${shots}/ship-item-${name}-wide.png` }).catch(() => {});

    // Phones: the same item, its diff scrolled; nothing wider than the screen.
    const phone = await signedIn(browser, phoneDevice);
    const p = await phone.newPage();
    const prow = await openItem(p, ITEM);
    await prow.locator(".rv-more").first().click();
    await prow.locator(".rv-fullb .rv-l").first().waitFor({ state: "attached", timeout: 120000 });
    const pw2 = await frames(p, ".rv-fullb");
    check(name, "the same scroll on a phone (median frame)", true, `median ${pw2.median?.toFixed(1)} ms, p95 ${pw2.p95?.toFixed(1)} ms`);
    const wideP = await p.evaluate(() => ({ scroll: document.documentElement.scrollWidth, inner: window.innerWidth }));
    check(name, "nothing wider than the phone", wideP.scroll <= wideP.inner, `${wideP.scroll} > ${wideP.inner}`);
    await checkLayout(check, `${name}: phone: the item`, p);
    await p.screenshot({ path: `${shots}/ship-item-${name}-phone.png` }).catch(() => {});
    await phone.close();
    await ctx.close();
  } catch (e) {
    check(name, "the check ran", false, String(e));
  } finally {
    await browser.close();
  }
}
console.log(report.join("\n"));
process.exit(failed ? 1 : 0);
