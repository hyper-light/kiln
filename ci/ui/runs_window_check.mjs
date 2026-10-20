// W21-AC04's page half, against signin_setup.py's server with a mock of a million runs behind
// `/api/runs` (pages of fifty, newest first, as the server pages them), in Chromium and WebKit,
// wide and on phones: the runs list grows to 5,000 runs as it scrolls, renders only the rows near
// the viewport, and anywhere it is scrolled to there are rows under the middle of the screen.
//
//     docker run --rm --network host -e SIGNIN="$(cat /tmp/signin.json)" \
//       -v $PWD/ci/ui:/check:ro -v <dir with node_modules/playwright-core>:/pw:ro -v /tmp/runs-shots:/shots \
//       mcr.microsoft.com/playwright:v1.64.1-noble node /check/runs_window_check.mjs
import { chromium, webkit, devices } from "/pw/node_modules/playwright-core/index.mjs";

const cfg = JSON.parse(process.env.SIGNIN);
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
    await new Promise((r) => setTimeout(r, 100));
  }
  return false;
}
const overflow = (page) => page.evaluate(() => document.scrollingElement.scrollWidth - window.innerWidth);

/** A million runs, newest first: run n was made at TOP - (MILLION - n) seconds. */
const MILLION = 1_000_000;
const TOP = 1_800_000_000;
function run(n) {
  const at = TOP - (MILLION - n);
  return { id: `run${n}`, repo_id: "mock", repo_name: "acme/people", number: n, pipeline: ".github/workflows/ci.yml", event: "push", branch: "main", sha: n.toString(16).padStart(40, "0"), message: `Change ${n}`, author: "bench", status: "success", created_at: at, started_at: at, finished_at: at + 60, error: null, concurrency_group: null };
}

for (const [name, type, phone] of [
  ["chromium", chromium, devices["Pixel 7"]],
  ["webkit", webkit, devices["iPhone 13"]],
]) {
  const browser = await type.launch();
  try {
    for (const [tag, options] of [
      ["wide", { viewport: { width: 1440, height: 900 } }],
      ["phone", phone],
    ]) {
      const ctx = await browser.newContext(options);
      // Signed in on a page of its own, as the other checks do: the list's page starts signed in.
      const r = await ctx.request.post(`${cfg.base}/api/users/lead/link`, { headers: { Authorization: `Bearer ${cfg.admin}` } });
      const signIn = await ctx.newPage();
      await signIn.goto((await r.json()).url);
      await signIn.close();
      const page = await ctx.newPage();
      const errors = [];
      page.on("pageerror", (e) => errors.push(String(e)));
      let served = 0;
      await page.route(/\/api\/runs(\?|$)/, async (route) => {
        const u = new URL(route.request().url());
        const limit = Number(u.searchParams.get("limit") ?? 50);
        const before = Number(u.searchParams.get("before") ?? TOP + 1);
        // The newest run made before `before`, and the forty-nine before it.
        const newest = MILLION - (TOP - Math.min(before - 1, TOP));
        const list = [];
        for (let n = newest; n > newest - limit && n > 0; n--) list.push(run(n));
        served += list.length;
        await route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify(list) });
      });
      await page.goto(`${cfg.base}/runs`);
      await page.locator("table.data-table tbody tr[data-href]").first().waitFor({ timeout: 30000 });
      // Scrolled to the end until 5,000 runs have come.
      const grown = await until(async () => {
        await page.evaluate(() => window.scrollTo(0, document.scrollingElement.scrollHeight));
        return served >= 5000;
      }, 120000);
      check(name, `${tag}: the list grew to 5,000 runs as it scrolled`, grown, String(served));
      const rendered = await page.locator("table.data-table tbody tr[data-href]").count();
      check(name, `${tag}: only the rows near the viewport are rendered`, rendered > 0 && rendered < 200, `${rendered} of ${served}`);
      // Anywhere: rows under the middle of the screen, a frame or two after the jump.
      const blank = [];
      for (const at of [0.05, 0.25, 0.5, 0.75, 0.95, 0.4, 0.1]) {
        const where = await page.evaluate(async (f) => {
          window.scrollTo(0, Math.floor(document.scrollingElement.scrollHeight * f));
          await new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)));
          const el = document.elementFromPoint(Math.floor(window.innerWidth / 2), Math.floor(window.innerHeight / 2));
          return el?.closest("tr[data-href]")?.getAttribute("aria-label") ?? null;
        }, at);
        if (!where) blank.push(at);
      }
      check(name, `${tag}: scrolled anywhere, rows fill the screen`, blank.length === 0, blank.length ? `blank at ${blank.join(", ")}` : "");
      check(name, `${tag}: the page fits`, (await overflow(page)) <= 0, String(await overflow(page)));
      await page.evaluate(() => window.scrollTo(0, Math.floor(document.scrollingElement.scrollHeight / 2)));
      await page.screenshot({ path: `${shots}/runs-window-${name}-${tag}.png` });
      check(name, `${tag}: no page errors`, errors.length === 0, errors.join("; "));
      await ctx.close();
    }
  } finally {
    await browser.close();
  }
}

console.log(report.join("\n"));
process.exit(failed ? 1 : 0);
