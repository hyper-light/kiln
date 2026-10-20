// Against signin_setup.py's server, in Chromium and WebKit, wide and on phones:
// - W10, W21: Settings → People and → Tokens. lead (the owner role) adds sam, gets sam a sign-in
//   link, grants and takes back a role, removes sam; makes a role of their own and removes it;
//   makes a token, sees it once, revokes it. dev (a viewer) has Tokens and no People.
// - W13-R09, W21-R08: the Runners page's pools. A pool added, heading for its minimum, warned of
//   as no runner can start it (W20-R03), edited in place, removed.
//
//     docker run --rm --network host -e SIGNIN="$(cat /tmp/signin.json)" \
//       -v $PWD/ci/ui:/check:ro -v <dir with node_modules/playwright-core>:/pw:ro -v /tmp/people-shots:/shots \
//       mcr.microsoft.com/playwright:v1.64.1-noble node /check/people_check.mjs
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
    await new Promise((r) => setTimeout(r, 200));
  }
  return false;
}
const overflow = (page) => page.evaluate(() => document.scrollingElement.scrollWidth - window.innerWidth);
async function linkIn(ctx, who) {
  const r = await ctx.request.post(`${cfg.base}/api/users/${who}/link`, { headers: { Authorization: `Bearer ${cfg.admin}` } });
  if (!r.ok()) throw new Error(`link: ${r.status()} ${await r.text()}`);
  const { url } = await r.json();
  const p = await ctx.newPage();
  await p.goto(url);
  await p.close();
}
const flow = async (label, f) => {
  try {
    await f();
  } catch (e) {
    check(label, "ran to its end", false, String(e).split("\n")[0]);
  }
};
const text = async (loc) => (await loc.innerText().catch(() => "")).replace(/\s+/g, " ");

for (const [name, type, phone] of [
  ["chromium", chromium, devices["Pixel 7"]],
  ["webkit", webkit, devices["iPhone 13"]],
]) {
  await flow(name, async () => {
    const browser = await type.launch();
    try {
      for (const [tag, options] of [
        ["wide", { viewport: { width: 1280, height: 900 } }],
        ["phone", phone],
      ]) {
        const ctx = await browser.newContext(options);
        const page = await ctx.newPage();
        const errors = [];
        page.on("pageerror", (e) => errors.push(String(e)));
        await linkIn(ctx, "lead");
        const sam = `sam-${name}-${tag}`;

        // People: add sam, a link, a grant given and taken back, sam removed.
        await page.goto(`${cfg.base}/settings#people.people`);
        await page.locator("#st-person-name").waitFor({ timeout: 30000 });
        const list = page.getByRole("list", { name: "People" });
        check(name, `${tag}: the people listed`, await until(async () => /lead/.test(await text(list)) && /dev/.test(await text(list))), await text(list));
        await page.locator("#st-person-name").fill(sam);
        await page.getByLabel("Their email").fill(`${sam}@example.com`);
        await page.getByRole("button", { name: "Add them" }).click();
        const row = list.getByRole("listitem").filter({ hasText: sam });
        check(name, `${tag}: sam added, a developer`, await until(async () => /developer/.test(await text(row))), await text(list));
        await row.getByRole("button", { name: "Sign-in link" }).click();
        const link = page.getByLabel(`${sam}'s sign-in link`);
        check(name, `${tag}: sam's sign-in link`, await until(async () => /\/api\/auth\/otp\?code=u\./.test(await link.inputValue())));
        await row.getByRole("button", { name: "Grants" }).click();
        const grants = page.getByRole("list", { name: `${sam}'s grants` });
        await grants.waitFor({ timeout: 15000 });
        const grantForm = row.locator("form");
        await grantForm.getByLabel("Role").selectOption("viewer");
        await grantForm.getByRole("button", { name: "Grant" }).click();
        check(name, `${tag}: a grant given`, await until(async () => /viewer/.test(await text(grants))), await text(grants));
        await grants.getByRole("listitem").filter({ hasText: "viewer" }).getByRole("button", { name: "Take it back" }).click();
        check(name, `${tag}: and taken back`, await until(async () => !/viewer/.test(await text(grants)) && /developer/.test(await text(grants))), await text(grants));
        check(name, `${tag}: the People page fits`, (await overflow(page)) <= 0, String(await overflow(page)));
        await page.screenshot({ path: `${shots}/people-${name}-${tag}.png`, fullPage: true });
        await row.getByRole("button", { name: "Remove" }).click();
        await page.getByRole("button", { name: `Remove ${sam}` }).click();
        check(name, `${tag}: sam removed`, await until(async () => !(await text(list)).includes(sam)), await text(list));

        // Roles: one of lead's own, made and removed.
        const role = `releaser-${tag}`;
        await page.locator("#st-role-name").fill(role);
        await page.getByRole("checkbox", { name: "runs.approve" }).check();
        await page.getByRole("checkbox", { name: "logs.read" }).check();
        await page.getByRole("button", { name: "Save the role" }).click();
        const roles = page.getByRole("list", { name: "Roles" });
        const roleRow = roles.getByRole("listitem").filter({ hasText: role });
        check(name, `${tag}: a role of one's own`, await until(async () => /runs\.approve/.test(await text(roleRow))), await text(roles));
        await roleRow.getByRole("button", { name: "Remove" }).click();
        check(name, `${tag}: and removed`, await until(async () => (await roleRow.count()) === 0));

        // Tokens: one made, shown once, revoked.
        await page.goto(`${cfg.base}/settings#tokens.mine`);
        await page.locator("#st-token-what").waitFor({ timeout: 30000 });
        const what = `Release script ${name} ${tag}`;
        await page.locator("#st-token-what").fill(what);
        await page.getByRole("button", { name: "Make the token" }).click();
        const shown = page.getByLabel("The new token");
        check(name, `${tag}: the new token shown once`, await until(async () => /^kiln_pat_/.test(await shown.inputValue())));
        const tokens = page.getByRole("list", { name: "Tokens" });
        const tokenRow = tokens.getByRole("listitem").filter({ hasText: what });
        check(name, `${tag}: the token listed, never used`, await until(async () => /never used/.test(await text(tokenRow))), await text(tokens));
        await tokenRow.getByRole("button", { name: "Revoke" }).click();
        await page.getByRole("button", { name: "Revoke it" }).click();
        check(name, `${tag}: and revoked`, await until(async () => /revoked/.test(await text(tokenRow))), await text(tokenRow));
        check(name, `${tag}: the Tokens page fits`, (await overflow(page)) <= 0, String(await overflow(page)));
        await page.screenshot({ path: `${shots}/tokens-${name}-${tag}.png`, fullPage: true });
        // Pools: one added, heading for its minimum, edited in place, removed.
        await page.goto(`${cfg.base}/runners`);
        const poolsPanel = page.locator("section.panel").filter({ has: page.locator(".panel-title", { hasText: /^Pools$/ }) });
        await poolsPanel.waitFor({ timeout: 30000 });
        const pool = `pool-${tag}`;
        await poolsPanel.getByRole("button", { name: "Add a pool" }).click();
        await page.locator("#pool-name").fill(pool);
        await page.locator("#pool-image").fill("kiln/runner-ubuntu-24.04");
        await page.locator("#pool-labels").fill("linux, docker");
        await page.locator("#pool-min").fill("1");
        await page.locator("#pool-max").fill("3");
        await page.getByRole("button", { name: "Add the pool" }).click();
        const poolRow = page.getByRole("table", { name: "Pools" }).getByRole("row").filter({ hasText: pool });
        check(name, `${tag}: the pool listed, heading for 1`, await until(async () => /\b1\b/.test(await text(poolRow.locator("td").nth(3)))), await text(poolRow));
        // No runner here: the save warns that none can start its executors (W20-R03).
        check(name, `${tag}: and warned that no runner can start it`, await until(async () => /no runner connected now announces the docker executor/.test(await text(poolsPanel))), await text(poolsPanel));
        await poolRow.getByRole("button", { name: pool }).click();
        await page.getByRole("button", { name: "Edit", exact: true }).click();
        await page.locator("#pool-max").fill("2");
        await page.getByRole("button", { name: "Save", exact: true }).click();
        check(name, `${tag}: edited in place`, await until(async () => /at most 2/.test(await text(poolsPanel))), await text(poolsPanel));
        check(name, `${tag}: the Runners page fits`, (await overflow(page)) <= 0, String(await overflow(page)));
        await page.screenshot({ path: `${shots}/pools-${name}-${tag}.png`, fullPage: true });
        await page.getByRole("button", { name: "Remove", exact: true }).click();
        await page.getByRole("button", { name: `Remove ${pool}` }).click();
        check(name, `${tag}: and removed`, await until(async () => (await poolRow.count()) === 0));
        check(name, `${tag}: no page errors`, errors.length === 0, errors.join("; "));
        await ctx.close();

        // dev, a viewer: Tokens, and no People.
        const dctx = await browser.newContext(options);
        const dpage = await dctx.newPage();
        await linkIn(dctx, "dev");
        await dpage.goto(`${cfg.base}/settings#tokens.mine`);
        await dpage.locator("#st-token-what").waitFor({ timeout: 30000 });
        check(name, `${tag}: dev has Tokens and no People`, (await dpage.locator("#st-person-name").count()) === 0 && (await dpage.getByText("Who signs in here, what each may do where").count()) === 0);
        await dctx.close();
      }
    } finally {
      await browser.close();
    }
  });
}

console.log(report.join("\n"));
process.exit(failed ? 1 : 0);
