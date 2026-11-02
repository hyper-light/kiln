// Refused messages on a runner's and an agent's pages (plan 01a17197252870e8b316f3eabee45505, step
// 9), against the mock server (ui/mock/server.ts, serving this build's UI): what kiln keeps of them
// is in its server's memory only, from wires a check can't make refuse yet, so the mock has them
// planted (ui/mock/refused.ts) and notes more when asked (`POST /api/mock/refuse`), telling the pages
// as kiln does. In Chromium and WebKit at 1440×900 and 390×844:
// - a runner with refusals: its "Refused messages" row in the fail tone, how many by wire and code
//   and the newest; and one noted now shows there within 5 s, without a reload;
// - a runner with none: "none", in no tone;
// - an agent with refusals: the flag in its head, which opens the list; and one noted now shows in
//   both within 5 s, without a reload;
// - an agent with none: no flag;
// then nothing on each page scrolling sideways and nothing fixed but the shell's own (layout.mjs),
// and a screenshot.
//
//     (cd ui && PORT=17081 MOCK_NO_AUTH=1 node --import tsx mock/server.ts) &
//     docker run --rm --network host -e MOCK='{"base": "http://127.0.0.1:17081"}' \
//       -v $PWD/ci/ui:/check:ro -v <dir with node_modules/playwright-core>:/pw:ro -v /tmp/refused-shots:/shots \
//       mcr.microsoft.com/playwright:v1.64.1-noble node /check/refused_check.mjs
import { chromium, webkit, devices } from "/pw/node_modules/playwright-core/index.mjs";
import { checkLayout, pageErrors } from "./layout.mjs";

const cfg = JSON.parse(process.env.MOCK);
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
const text = async (loc) => (await loc.innerText().catch(() => "")).replace(/\s+/g, " ").trim();
const flow = async (label, f) => {
  try {
    await f();
  } catch (e) {
    check(label, "ran to its end", false, String(e).split("\n")[0]);
  }
};

// ui/mock/refused.ts plants these; the others have none.
const RUNNER = "metal-ryzen-01";
const CLEAN_RUNNER = "metal-ryzen-02";
const AGENT = "fam-tests";
const CLEAN_AGENT = "fam-docs";
/** How many this check noted: each says a `what` of its own, so it's told apart from those before. */
let noted = 0;
async function refuse(params) {
  const r = await fetch(`${cfg.base}/api/mock/refuse?${new URLSearchParams(params)}`, { method: "POST" });
  if (!r.ok) throw new Error(`POST /api/mock/refuse: ${r.status}`);
}
/** Marks the page: still marked later, it wasn't loaded again. */
const mark = (page) => page.evaluate(() => (window.__refusedCheck = true));
const marked = (page) => page.evaluate(() => window.__refusedCheck === true).catch(() => false);
/** A runner's row in its details, by its label. */
const factRow = (page, runner, label) =>
  page
    .getByRole("region", { name: `${runner} in depth` })
    .locator(".facts > div")
    .filter({ has: page.locator("dt", { hasText: new RegExp(`^${label}$`) }) });

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
      const page = await ctx.newPage();
      const errors = pageErrors(page, cfg.base);
      // As it settles: the list fades in as it opens.
      const shot = (what) => page.screenshot({ path: `${shots}/${what}-${name}-${tag}.png`, fullPage: true, animations: "disabled" });

      await flow(`${who}: a runner's refused messages`, async () => {
        await page.goto(`${cfg.base}/runners?agent=${RUNNER}`);
        const row = factRow(page, RUNNER, "Refused messages");
        const words = row.locator("dd");
        const said = /^(\d+) \(runner unknown \d+, reach_link malformed 1, server_item malformed 1\) · newest (now|\S+ ago), variant \d+: AgentMsg has 51 variants$/;
        check(
          who,
          "a runner's row: how many by wire and code, the newest with when, what and why, in the fail tone",
          await until(async () => said.test(await text(words)) && (await row.getAttribute("data-tone")) === "fail", 20000),
          `${await row.getAttribute("data-tone").catch(() => null)}: ${await text(words)}`,
        );
        await checkLayout(check, `${who}: a runner's refused messages`, page);
        await shot("runner");
        const before = Number(said.exec(await text(words))?.[1] ?? NaN);
        const what = `variant ${100 + ++noted}`;
        await mark(page);
        const at = Date.now();
        await refuse({ runner: RUNNER, what });
        const shown = await until(async () => (await text(words)).startsWith(`${before + 1} (`) && (await text(words)).endsWith(`${what}: AgentMsg has 51 variants`), 5000);
        check(who, "a refusal noted for it shows on its page within 5 s, without a reload", shown && (await marked(page)), `${Date.now() - at} ms: ${await text(words)}`);
      });

      await flow(`${who}: a runner with none`, async () => {
        await page.goto(`${cfg.base}/runners?agent=${CLEAN_RUNNER}`);
        const row = factRow(page, CLEAN_RUNNER, "Refused messages");
        check(
          who,
          'a runner with none: "none", in no tone',
          await until(async () => (await text(row.locator("dd"))) === "none" && (await row.getAttribute("data-tone")) === null, 20000),
          `${await row.getAttribute("data-tone").catch(() => null)}: ${await text(row.locator("dd"))}`,
        );
      });

      await flow(`${who}: an agent's refused messages`, async () => {
        await page.goto(`${cfg.base}/agents/${AGENT}`);
        const head = page.locator("header.ag-head");
        const flag = head.getByRole("button", { name: /^\d+ messages? refused$/ });
        check(who, "an agent's flag, in its head", await until(async () => (await flag.count()) === 1, 20000), await text(head));
        await flag.click();
        const list = head.getByRole("region", { name: "Refused messages" });
        check(
          who,
          "the flag opens the list: how many by wire and code, then the newest first, each with when, what and why",
          await until(
            async () =>
              /^runner_payload unknown \d+, mcp_http malformed 1/.test(await text(list)) &&
              (await list.locator("li").count()) >= 3 &&
              /^(now|\S+ ago) runner_payload unknown update kind "\S+": no such update$/.test(await text(list.locator("li").first())) &&
              /mcp_http malformed tools\/call goals_add: unknown field `golas` at \/golas$/.test(await text(list.locator("li").last())),
            10000,
          ),
          await text(list),
        );
        check(who, "and says it's open", (await flag.getAttribute("aria-expanded")) === "true");
        await checkLayout(check, `${who}: an agent's refused messages`, page);
        await shot("agent");
        const before = Number(/^(\d+)/.exec(await text(flag))?.[1] ?? NaN);
        const what = `update kind "check-${++noted}"`;
        await mark(page);
        const at = Date.now();
        await refuse({ agent: AGENT, what });
        const shown = await until(async () => (await text(flag)) === `${before + 1} messages refused` && (await text(list.locator("li").first())).includes(`${what}: no such update`), 5000);
        check(
          who,
          "a refusal noted for it shows in the flag and the list within 5 s, without a reload",
          shown && (await marked(page)),
          `${Date.now() - at} ms: ${await text(flag)}; ${await text(list.locator("li").first())}`,
        );
        await page.keyboard.press("Escape");
        check(who, "Escape closes the list", await until(async () => (await list.count()) === 0, 5000));
      });

      await flow(`${who}: an agent with none`, async () => {
        await page.goto(`${cfg.base}/agents/${CLEAN_AGENT}`);
        const head = page.locator("header.ag-head");
        // Once its name is in, so is what it says of its messages.
        check(
          who,
          "an agent with none: no flag",
          (await until(async () => /Update the docs/.test(await text(head)), 20000)) && (await head.getByRole("button", { name: /refused/ }).count()) === 0,
          await text(head),
        );
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
