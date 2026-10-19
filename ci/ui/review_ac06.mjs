// W14-AC06, in Chromium and WebKit against a real kiln server (review_setup.py made its diff): the
// conversations inbox, next/previous unresolved, viewed files, word diffs, moved blocks, the
// generated file folded, the image diff, a 10,000-line diff's scrolling, unified on phones.
import { chromium, webkit, devices } from "/pw/node_modules/playwright-core/index.mjs"; // playwright-core 1.64.1

const cfg = JSON.parse(process.env.AC06);
const report = [];
let failed = 0;
const check = (browser, what, ok, detail = "") => {
  report.push(`${ok ? "ok  " : "FAIL"} ${browser}: ${what}${detail ? ` (${detail})` : ""}`);
  if (!ok) failed++;
};

// As the author signs in: a one-time link the server's admin makes for them.
async function signedIn(browser, options) {
  const ctx = await browser.newContext(options);
  const r = await ctx.request.post(cfg.base + "/api/users/dev/link", { headers: { Authorization: `Bearer ${cfg.admin}` } });
  if (!r.ok()) throw new Error(`link: ${r.status()} ${await r.text()}`);
  const { url } = await r.json();
  const page = await ctx.newPage();
  await page.goto(url);
  await page.close();
  return ctx;
}

const fileOf = (page, path) => page.locator(".rv-file").filter({ has: page.locator(".rv-path", { hasText: path }) }).first();

for (const [name, type, phoneDevice] of [
  ["chromium", chromium, devices["Pixel 7"]],
  ["webkit", webkit, devices["iPhone 13"]],
]) {
  const browser = await type.launch();
  try {
    const ctx = await signedIn(browser, { viewport: { width: 1440, height: 900 } });
    const page = await ctx.newPage();
    const errors = [];
    page.on("pageerror", (e) => errors.push(String(e)));
    const bad = [];
    page.on("response", (r) => {
      if (r.status() >= 400) bad.push(`${r.status()} ${r.url().replace(cfg.base, "")}`);
    });

    // Your conversations: the reviewer's question waits for your reply.
    await page.goto(cfg.base + "/review");
    const inbox = await page.getByText("Why the American spelling?").first().waitFor({ timeout: 60000 }).then(() => true, () => false);
    check(name, "the inbox shows the thread waiting for your reply", inbox);

    // The diff, every file unviewed (a run before may have left one ticked).
    for (const file of ["big.txt", "moved.rs", "words.txt", "logo.png", "dist/gen.js"]) {
      await ctx.request.post(cfg.base + "/api/diffs/D1/viewed", { data: { file, viewed: false } });
    }
    await page.goto(cfg.base + "/review/D1");
    await page.locator(".rv-file").first().waitFor({ timeout: 120000 });
    await fileOf(page, "words.txt").scrollIntoViewIfNeeded({ timeout: 60000 });
    await fileOf(page, "words.txt").locator(".rv-row").first().waitFor({ state: "attached", timeout: 60000 });
    const marks = await fileOf(page, "words.txt").locator(".rv-mark").allTextContents();
    check(name, "word diffs mark only the changed word", marks.some((m) => m.includes("colo")) && marks.every((m) => m.length < 12), JSON.stringify(marks));
    const moved = await fileOf(page, "moved.rs").locator(".rv-moved").allTextContents();
    check(name, "the moved block says where it went", moved.some((m) => /moved (to|from)/.test(m)), JSON.stringify(moved));
    const gen = fileOf(page, "dist/gen.js");
    const genChip = await gen.locator(".rv-chip", { hasText: "generated" }).count();
    const genOpen = await gen.locator(".rv-file-toggle").getAttribute("aria-expanded");
    check(name, "the generated file is folded and says so", genChip === 1 && genOpen === "false", `chip ${genChip}, expanded ${genOpen}`);
    await fileOf(page, "logo.png").scrollIntoViewIfNeeded({ timeout: 30000 });
    const imgs = fileOf(page, "logo.png").locator(".rv-images img");
    await imgs.first().waitFor({ state: "attached", timeout: 30000 }).catch(() => {});
    const loaded = await imgs.evaluateAll((list) =>
      Promise.all(list.map((i) => i.decode().then(() => `${i.naturalWidth}x${i.naturalHeight}`, () => `unloaded ${i.getAttribute("src")}`))),
    );
    check(name, "the image diff shows both sides", loaded.length === 2 && loaded.every((l) => l === "16x16"), JSON.stringify(loaded));

    // Viewed files: ticked, and still ticked after a reload.
    const tick = () => fileOf(page, "words.txt").locator(".rv-viewed input");
    const until = async (f, ms = 15000) => {
      const end = Date.now() + ms;
      while (Date.now() < end) {
        if (await f()) return true;
        await page.waitForTimeout(250);
      }
      return false;
    };
    await tick().click();
    const ticked = await until(() => tick().isChecked());
    await page.reload();
    await fileOf(page, "words.txt").waitFor({ state: "attached", timeout: 120000 });
    const still = await until(() => tick().isChecked());
    check(name, "a viewed file stays viewed", ticked && still, `ticked ${ticked}, after a reload ${still}`);
    await tick().click();
    await until(async () => !(await tick().isChecked()));

    // Next unresolved: `n` brings the thread into view.
    await page.reload();
    await page.locator(".rv-file").first().waitFor({ timeout: 120000 });
    await page.evaluate(() => window.scrollTo(0, document.body.scrollHeight));
    await page.locator("body").click({ position: { x: 5, y: 5 } }).catch(() => {});
    await page.keyboard.press("n");
    await page.waitForTimeout(1500);
    const inView = await page.evaluate(() => {
      const t = document.querySelector('[id^="rv-t-"]');
      if (!t) return "no thread element";
      const r = t.getBoundingClientRect();
      return r.top >= 0 && r.bottom <= window.innerHeight ? true : `at ${Math.round(r.top)} of ${window.innerHeight}`;
    });
    check(name, "n moves to the unresolved thread", inView === true, String(inView));

    // The 10,000-line file: frames while it scrolls.
    const big = fileOf(page, "big.txt");
    await big.scrollIntoViewIfNeeded();
    const frames = await page.evaluate(async () => {
      const times = [];
      let last = performance.now();
      const end = last + 4000;
      await new Promise((resolve) => {
        const step = (t) => {
          times.push(t - last);
          last = t;
          window.scrollBy(0, 120);
          if (t < end) requestAnimationFrame(step);
          else resolve();
        };
        requestAnimationFrame(step);
      });
      times.shift();
      times.sort((a, b) => a - b);
      return { n: times.length, median: times[times.length >> 1], p95: times[Math.floor(times.length * 0.95)] };
    });
    const rows = await big.locator(".rv-row").count();
    check(name, "a 10,000-line diff scrolls at 60 fps (median frame ≤ 17.5 ms)", frames.median <= 17.5, `median ${frames.median?.toFixed(1)} ms, p95 ${frames.p95?.toFixed(1)} ms over ${frames.n} frames; ${rows} rows in the DOM`);
    check(name, "no script errors on the desktop", errors.length === 0, errors.join(" | "));
    check(name, "no failed requests", bad.length === 0, bad.slice(0, 5).join(", "));

    // Phones: unified, nothing wider than the screen.
    const phone = await signedIn(browser, phoneDevice);
    const p = await phone.newPage();
    await p.goto(cfg.base + "/review/D1");
    await p.locator(".rv-file").first().waitFor({ timeout: 120000 });
    await fileOf(p, "words.txt").locator(".rv-row").first().waitFor({ state: "attached", timeout: 120000 });
    const split = await p.locator(".rv-hunks[data-split]").count();
    const wide = await p.evaluate(() => ({ scroll: document.documentElement.scrollWidth, inner: window.innerWidth }));
    check(name, "unified on a phone", split === 0, `${split} split`);
    check(name, "nothing wider than the phone", wide.scroll <= wide.inner, `${wide.scroll} > ${wide.inner}`);
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
