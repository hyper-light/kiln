// Against signin_setup.py's server, in Chromium and WebKit, wide and on phones:
// - W10, W21: Settings → People and → Tokens. lead (the owner role) adds sam, gets sam a sign-in
//   link, grants and takes back a role, removes sam; makes a role of their own and removes it;
//   makes a token, sees it once, revokes it. dev (a viewer) has Tokens and no People.
// - W13-R09, W21-R08: the Runners page's pools. A pool added, heading for its minimum, warned of
//   as no runner can start it (W20-R03), edited in place, removed.
// - W10-R12: Settings → OIDC says how the issuer is reached, and fits a phone.
// - W09-R11: a repository's secrets on the Repositories page, a workflow's variables in its
//   editor, each set and removed in place.
// - W21-R06: the editor's saving, the patch of an edit, a diff opened on the trunk and a commit to
//   a branch; W02-R12: a label no runner has, warned of where it is.
// - W21-R01: the editor's keys from the schema, on hover (desktop) and as one is typed; W21-R06:
//   the plan preview's matrix as written, and the flow view's edits in the YAML.
// - W21-R07: an action's inputs and outputs edited in place, and "Try it" starting its run.
//
//     docker run --rm --network host -e SIGNIN="$(cat /tmp/signin.json)" \
//       -v $PWD/ci/ui:/check:ro -v <dir with node_modules/playwright-core>:/pw:ro -v /tmp/people-shots:/shots \
//       mcr.microsoft.com/playwright:v1.64.1-noble node /check/people_check.mjs
import { readFileSync } from "node:fs";
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
        // Settings → CI → OIDC (W10-R12): how the issuer is reached, laid out on this screen.
        await page.goto(`${cfg.base}/settings#ci.oidc`);
        const oidc = page.locator("[id='ci.oidc']").first();
        await oidc.waitFor({ timeout: 30000 });
        await until(async () => /From the internet/.test(await text(oidc)), 30000);
        check(name, `${tag}: how the issuer is reached`, /From the internet No: its URL is this network's/.test(await text(oidc)) && /AWS and Azure fetch/.test(await text(oidc)), (await text(oidc)).slice(0, 300));
        check(name, `${tag}: the OIDC settings fit`, (await overflow(page)) <= 0, String(await overflow(page)));
        await oidc.screenshot({ path: `${shots}/oidc-${name}-${tag}.png` });
        // A repository's own secrets, on the Repositories page (W09-R11): set, never shown, removed.
        await page.goto(`${cfg.base}/repos`);
        // Its name opens it, as a tap does (the row's centre may be one of its buttons).
        await page.locator("table tbody tr", { hasText: "acme/people" }).first().locator("td").first().click();
        const repoPanel = page.getByRole("region", { name: "Its secrets and variables" });
        await repoPanel.locator("#st-secret-name").waitFor({ timeout: 30000 });
        const key = `REPO_${name}_${tag}`.toUpperCase();
        await repoPanel.locator("#st-secret-name").fill(key);
        await repoPanel.locator("#st-secret-value").fill("repo-secret-value-41");
        await repoPanel.getByRole("button", { name: "Save the secret" }).click();
        const listed = await until(async () => (await text(repoPanel.getByRole("list", { name: "Secrets" }))).includes(key));
        check(name, `${tag}: a repository's secret, set on its page`, listed && !(await page.content()).includes("repo-secret-value-41"), await text(repoPanel));
        check(name, `${tag}: the Repositories page fits`, (await overflow(page)) <= 0, String(await overflow(page)));
        await page.screenshot({ path: `${shots}/repo-secrets-${name}-${tag}.png`, fullPage: true });
        await repoPanel.getByRole("list", { name: "Secrets" }).getByRole("button", { name: "Remove", exact: true }).first().click();
        await repoPanel.getByRole("button", { name: `Remove ${key}` }).click();
        check(name, `${tag}: and removed`, await until(async () => !(await text(repoPanel)).includes(key)), await text(repoPanel));
        // A workflow's own variables, in its editor.
        await page.goto(`${cfg.base}/workflows?repo=acme%2Fpeople&path=.kiln%2Fci.yaml`);
        const wfPanel = page.getByRole("region", { name: "This workflow's secrets and variables" });
        await wfPanel.locator("#st-var-name").waitFor({ timeout: 30000 });
        await wfPanel.locator("#st-var-name").fill("stage");
        await wfPanel.locator("#st-var-value").fill("blue");
        await wfPanel.getByRole("button", { name: "Save the variable" }).click();
        const vars = wfPanel.getByRole("list", { name: "Variables" });
        check(name, `${tag}: a workflow's variable, set in its editor`, await until(async () => /STAGE[\s\S]*blue/.test(await text(vars))), await text(wfPanel));
        check(name, `${tag}: the editor's page fits`, (await overflow(page)) <= 0, String(await overflow(page)));
        await page.screenshot({ path: `${shots}/workflow-secrets-${name}-${tag}.png`, fullPage: true });
        await vars.getByRole("button", { name: "Remove", exact: true }).first().click();
        check(name, `${tag}: and removed`, await until(async () => (await vars.count()) === 0 || !/STAGE/.test(await text(vars))), await text(wfPanel));
        // What only the server knows (W02-R12): no runner here has `local`, said where it is.
        const warned = page.locator(".cm-lintRange-warning", { hasText: "local" });
        check(name, `${tag}: a label no runner has, warned of in place`, await until(async () => (await warned.count()) > 0, 20000), await text(page.locator("section.panel").first()));
        // The editor's saving (W21-R06): the patch of what changed, and a diff opened on the trunk.
        await page.locator(".cm-content").first().click();
        await page.keyboard.press("Control+End");
        await page.keyboard.type(`# checked in ${name} ${tag}\n`);
        const patchButton = page.getByRole("button", { name: "The patch", exact: true });
        await patchButton.waitFor({ timeout: 15000 });
        const [download] = await Promise.all([page.waitForEvent("download"), patchButton.click()]);
        const patch = readFileSync(await download.path(), "utf8");
        check(name, `${tag}: the patch of what changed`, /^\+\+\+ b\/\.kiln\/ci\.yaml$/m.test(patch) && new RegExp(`^\\+# checked in ${name} ${tag}$`, "m").test(patch), patch.slice(0, 300));
        await page.getByRole("button", { name: "Open a diff", exact: true }).click();
        const opened = page.getByRole("status").filter({ hasText: "opened: it lands through review" });
        check(name, `${tag}: a diff opened on the trunk`, await until(async () => (await opened.count()) > 0, 30000), await text(page.locator("main").first()));
        // Or a commit to a branch, taken as a push of it would be.
        await page.getByRole("button", { name: "To a branch", exact: true }).click();
        const onto = `edit-${name}-${tag}`;
        await page.locator("#wf-save-branch").fill(onto);
        await page.getByRole("button", { name: "Commit", exact: true }).click();
        const committed = page.getByRole("status").filter({ hasText: `Committed to ${onto}` });
        check(name, `${tag}: committed to a branch`, await until(async () => (await committed.count()) > 0, 30000), await text(page.getByRole("status").first()));
        // Said on a line of its own width, the buttons beside it or below.
        const said = await committed.first().boundingBox();
        check(name, `${tag}: and said in a line that reads`, (said?.width ?? 0) >= 200, JSON.stringify(said));
        check(name, `${tag}: the editor fits, saved`, (await overflow(page)) <= 0, String(await overflow(page)));
        const cut = await page.locator("section.panel").first().evaluate((el) => el.scrollWidth - el.clientWidth);
        check(name, `${tag}: nothing in the editor's panel is cut off`, cut <= 0, String(cut));
        // The plan preview's matrix (W21-R06): its axes and exclude as rows, the legs it makes.
        const matrix = page.getByRole("table", { name: "Its matrix" });
        check(name, `${tag}: the matrix as written, its legs counted`, await until(async () => /os linux · mac/i.test(await text(matrix)) && /exclude os: mac, n: 1/i.test(await text(matrix)) && /3 legs · 1 removed/i.test(await text(page.getByRole("region", { name: "Job build" })))), await text(page.getByRole("region", { name: "Job build" })));
        // The editor knows the schema's keys (W21-R01): what one is, on hover; a step's, as typed.
        if (tag === "wide") {
          await page.locator(".cm-content").getByText("runs-on", { exact: true }).first().hover();
          const tip = page.locator(".cm-tooltip-hover .cm-hover-words");
          check(name, `${tag}: a key's hover says what it is`, await until(async () => /label/i.test(await text(tip))), await text(tip));
          await page.mouse.move(0, 0);
        }
        await page.locator(".cm-line", { hasText: "- run: echo hi" }).first().click();
        await page.keyboard.press("End");
        // The new line and its indentation as text: emulated Android (the phone's Chromium) sends no
        // Enter CodeMirror takes, as a real keyboard's input events would.
        await page.keyboard.insertText("\n        ");
        await page.keyboard.type("shel");
        const offered = page.locator(".cm-tooltip-autocomplete li", { hasText: "shell" }).first();
        check(name, `${tag}: a step's keys offered as one is typed`, await until(async () => (await offered.count()) > 0), await text(page.locator(".cm-tooltip-autocomplete").first()));
        await offered.click();
        check(name, `${tag}: and the one chosen written`, await until(async () => /shell: /.test(await page.locator(".cm-content").innerText())), await text(page.locator(".cm-content")));
        // The flow view (W21-R06): a job added, what it needs chosen, an axis's values; the YAML
        // has them, its comment kept, and the plan follows.
        await page.getByRole("group", { name: "View" }).getByRole("button", { name: "Flow", exact: true }).click();
        const flowView = page.getByRole("region", { name: "Its flow" });
        const buildCard = flowView.getByRole("region", { name: "Job build" });
        check(name, `${tag}: the flow view, a job and where it runs`, await until(async () => /local/.test(await text(buildCard)) && /echo hi/.test(await text(buildCard))), await text(buildCard));
        await page.getByLabel("A new job").fill("lint");
        await page.getByRole("button", { name: "Add the job", exact: true }).click();
        const lintCard = flowView.getByRole("region", { name: "Job lint" });
        await lintCard.waitFor({ timeout: 15000 });
        await lintCard.getByLabel("Add a need").selectOption("build");
        const stage2 = flowView.getByRole("region", { name: "Stage 2" });
        check(name, `${tag}: a job added, needing build, in the second stage`, await until(async () => (await stage2.getByRole("region", { name: "Job lint" }).count()) > 0), await text(flowView));
        const osValues = buildCard.getByLabel("Values of os");
        await osValues.fill("linux, mac, windows");
        await osValues.press("Enter");
        check(name, `${tag}: an axis's values, and the plan follows`, await until(async () => /5 legs · 1 removed/i.test(await text(page.locator("main").first()))), await text(page.locator("main").first()));
        check(name, `${tag}: the flow view fits`, (await overflow(page)) <= 0, String(await overflow(page)));
        await page.screenshot({ path: `${shots}/workflow-flow-${name}-${tag}.png`, fullPage: true });
        await page.getByRole("group", { name: "View" }).getByRole("button", { name: "YAML", exact: true }).click();
        const yamlText = async () => await page.locator(".cm-content").innerText();
        check(
          name,
          `${tag}: the YAML has it all, its comment kept`,
          await until(async () => {
            const y = await yamlText();
            return /lint:/.test(y) && /needs: build/.test(y) && /os: \[linux, mac, windows\]/.test(y) && new RegExp(`# checked in ${name} ${tag}`).test(y);
          }),
          (await yamlText()).slice(0, 600),
        );
        await page.screenshot({ path: `${shots}/workflow-saved-${name}-${tag}.png`, fullPage: true });
        // An action in the same editor (W21-R07): its inputs and outputs edited in place, the YAML
        // with them, and tried as it is there.
        await page.goto(`${cfg.base}/workflows?repo=acme%2Fpeople&path=.github%2Factions%2Fgreet%2Faction.yml`);
        await page.locator(".cm-content").first().waitFor({ timeout: 30000 });
        await page.getByRole("group", { name: "View" }).getByRole("button", { name: "Inputs and outputs", exact: true }).click();
        const io = page.getByRole("region", { name: "Its inputs and outputs" });
        check(name, `${tag}: an action's inputs, each with what it is`, await until(async () => /who/.test(await text(io)) && (await io.getByLabel("Its default").inputValue()) === "world"), await text(io));
        await io.getByLabel("A new output").fill("greeting");
        await io.getByRole("region", { name: "Outputs" }).getByLabel("What it is").last().fill("What it said.");
        await io.getByRole("button", { name: "Add the output", exact: true }).click();
        const out = io.getByRole("listitem", { name: "Output greeting" });
        await out.waitFor({ timeout: 15000 });
        await out.getByLabel("Its value").fill("${{ steps.say.outputs.greeting }}");
        await out.getByLabel("Its value").press("Tab");
        await io.getByRole("listitem", { name: "Input who" }).getByLabel("Required").check();
        await page.getByRole("group", { name: "View" }).getByRole("button", { name: "YAML", exact: true }).click();
        const actionYaml = async () => await page.locator(".cm-content").innerText();
        check(
          name,
          `${tag}: the YAML has them, its comment kept`,
          await until(async () => {
            const y = await actionYaml();
            return /outputs:\s+greeting:\s+description: What it said\.\s+value: \$\{\{ steps\.say\.outputs\.greeting \}\}/.test(y) && /required: true/.test(y) && /who: {2}# whom/.test(y);
          }),
          (await actionYaml()).slice(0, 600),
        );
        const tryPane = page.locator("section.panel").filter({ has: page.getByRole("button", { name: "Try it", exact: true }) });
        await tryPane.getByLabel("who (required)").fill(`${name} ${tag}`);
        await tryPane.getByRole("button", { name: "Try it", exact: true }).click();
        check(name, `${tag}: tried: its run, started`, await until(async () => /Run #\d+/.test(await text(tryPane.getByRole("region", { name: "Its run" }))), 30000), await text(tryPane));
        check(name, `${tag}: the action's page fits`, (await overflow(page)) <= 0, String(await overflow(page)));
        await page.screenshot({ path: `${shots}/action-${name}-${tag}.png`, fullPage: true });
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
