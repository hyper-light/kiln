// W21-R10 (docs/ci.md W15-R08, R09), against environments_setup.py's repositories, in Chromium and
// WebKit at 1440×900 and 390×844:
// - the board: every environment of acme/stages a row, with what it deploys to and its rules, the
//   deployment to production waiting for its review, and the file's environment's kind;
// - a starter set made from the board, in a repository of its own;
// - an environment's page: its kind changed in place (host to container, and back), saved and
//   read again; the file's environment read-only; production's wait, its empty history and its
//   secrets;
// each page with W21-AC01's two asserts (layout.mjs) and a screenshot.
//
//     docker run --rm --network host -e ENVS="$(cat /tmp/envs.json)" \
//       -v $PWD/ci/ui:/check:ro -v <dir with node_modules/playwright-core>:/pw:ro -v /tmp/envs-shots:/shots \
//       mcr.microsoft.com/playwright:v1.64.1-noble node /check/environments_check.mjs
import { chromium, webkit, devices } from "/pw/node_modules/playwright-core/index.mjs";
import { checkLayout } from "./layout.mjs";

const cfg = JSON.parse(process.env.ENVS);
const shots = "/shots";
const report = [];
let failed = 0;
const check = (b, what, ok, detail = "") => {
  report.push(`${ok ? "ok  " : "FAIL"} ${b}: ${what}${detail ? ` (${detail})` : ""}`);
  if (!ok) failed++;
};
async function until(f, ms = 20000) {
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
const envUrl = (name) => `${cfg.base}/deployments/environments/${encodeURIComponent(name)}?repo=${encodeURIComponent(cfg.repo)}`;

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
      const errors = [];
      page.on("pageerror", (e) => errors.push(String(e)));
      const shot = (what) => page.screenshot({ path: `${shots}/${what}-${name}-${tag}.png`, fullPage: true });

      await flow(`${who}: the board`, async () => {
        await page.goto(`${cfg.base}/deployments/environments`);
        const rows = page.getByRole("list", { name: `${cfg.repo}'s environments` });
        const row = (env) => rows.getByRole("listitem").filter({ has: page.getByRole("link", { name: env, exact: true }) });
        check(who, "the board: acme/stages's environments, a row each", await until(async () => (await rows.getByRole("listitem").count()) >= 5), await text(rows));
        check(who, "production waits for its review", await until(async () => /Waiting for a review to deploy to production/.test(await text(row("production")))), await text(row("production")));
        check(
          who,
          "production's rules",
          await until(async () => /owner reviews/.test(await text(row("production"))) && /main only/.test(await text(row("production")))),
          await text(row("production")),
        );
        check(who, "web deploys on its runners", await until(async () => /Host · label deploy-web/.test(await text(row("web")))), await text(row("web")));
        check(who, "the file's environment, its kind from its clusters", await until(async () => /Kubernetes · in-cluster \/ web/.test(await text(row("cluster")))), await text(row("cluster")));
        await checkLayout(check, `${who}: the board`, page);
        await shot("environments-board");
      });

      await flow(`${who}: a starter set`, async () => {
        const fresh = cfg.fresh[`${name}-${tag}`];
        await page.getByLabel("Repository").selectOption(fresh);
        await page.getByRole("button", { name: "Make dev, staging and production" }).click();
        const rows = page.getByRole("list", { name: `${fresh}'s environments` });
        check(who, "a starter set, made in one step", await until(async () => /dev/.test(await text(rows)) && /staging/.test(await text(rows)) && /production/.test(await text(rows))), await text(rows));
      });

      await flow(`${who}: an environment, changed in place`, async () => {
        for (const [kind, label] of [
          ["Container", "Container"],
          ["Host", "Host"],
        ]) {
          await page.goto(envUrl("web"));
          await page.getByRole("group", { name: "Kind" }).getByRole("button", { name: kind, exact: true }).click();
          await page.getByRole("button", { name: "Save web" }).click();
          check(who, `web saved as ${kind}`, await until(async () => /Saved web/.test(await text(page.locator("body")))));
          await page.reload();
          check(who, `and read again: ${label}`, await until(async () => new RegExp(`Kind ${label}`).test(await text(page.locator("dl.facts")))), await text(page.locator("dl.facts")));
        }
        check(who, "its runners kept", (await page.getByLabel("Runners").inputValue()) === "label deploy-web");
        await checkLayout(check, `${who}: an environment`, page);
        await shot("environment-web");
      });

      await flow(`${who}: the file's environment`, async () => {
        await page.goto(envUrl("cluster"));
        check(who, "the file's environment is changed in the file", await until(async () => /Defined in .kiln\/environments.yaml on the trunk/.test(await text(page.locator("main")))));
        check(who, "and isn't saved here", (await page.getByRole("button", { name: "Save cluster" }).count()) === 0);
        await checkLayout(check, `${who}: the file's environment`, page);
      });

      await flow(`${who}: production`, async () => {
        await page.goto(envUrl("production"));
        const main = page.locator("main");
        check(who, "production: what waits there", await until(async () => /toprod/.test(await text(page.getByRole("list", { name: "Waiting to deploy to production" })))), await text(main).then((t) => t.slice(0, 300)));
        check(who, "its history, empty", await until(async () => /No deployments yet/.test(await text(main))));
        check(who, "its secrets and variables", await until(async () => /production secrets/i.test(await text(main)) && /production variables/i.test(await text(main))), (await text(main)).slice(-300));
        await checkLayout(check, `${who}: production`, page);
        await shot("environment-production");
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
