// W21-AC02's second part (docs/ci.md), against signin_setup.py's server in Chromium and WebKit:
// the editor completes `needs.`, `needs.<job>.outputs.`, `steps.`, `matrix.`, `secrets.`, `uses:`
// and a step's `with:` (the inputs of the action it uses) from a fixture; a diagnostic's fix is
// taken from it (W21-R01's quick actions); and a commented file goes through the flow view and
// back with nothing changed, then with one edit made and its comments kept. (The first part,
// every negative-corpus file at its range, is ci/ui/editor_ranges_check.ts.)
//
//     docker run --rm --network host -e SIGNIN="$(cat /tmp/signin.json)" \
//       -v $PWD/ci/ui:/check:ro -v <dir with node_modules/playwright-core>:/pw:ro -v /tmp/editor-shots:/shots \
//       mcr.microsoft.com/playwright:v1.64.1-noble node /check/editor_check.mjs
import { chromium, webkit } from "/pw/node_modules/playwright-core/index.mjs";
import { checkLayout } from "./layout.mjs";

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
    await new Promise((r) => setTimeout(r, 150));
  }
  return false;
}

const FIXTURE = `# The fixture W21-AC02 completes in.
on: push
jobs:
  build:   # first
    runs-on: local
    strategy:
      matrix:
        os: [linux, mac]
    outputs:
      version: \${{ steps.v.outputs.version }}
    steps:
      - id: v
        run: echo "version=1" >> "$GITHUB_OUTPUT"
      - run: echo build
  test:
    needs: build
    runs-on: local
    steps:
      - run: echo test
`;

for (const [name, type] of [
  ["chromium", chromium],
  ["webkit", webkit],
]) {
  const browser = await type.launch();
  try {
    const ctx = await browser.newContext({ viewport: { width: 1440, height: 900 } });
    const r = await ctx.request.post(`${cfg.base}/api/users/lead/link`, { headers: { Authorization: `Bearer ${cfg.admin}` } });
    const signIn = await ctx.newPage();
    await signIn.goto((await r.json()).url);
    await signIn.close();
    const page = await ctx.newPage();
    const errors = [];
    page.on("pageerror", (e) => errors.push(String(e)));
    await page.goto(`${cfg.base}/workflows?repo=acme%2Fpeople&path=.kiln%2Fci.yaml`);
    const content = page.locator(".cm-content").first();
    await content.waitFor({ timeout: 30000 });
    const doc = async () => (await content.innerText()).replace(/ /g, " ");
    // The fixture in place of the file, checked (the completions read what the check found).
    const load = async (text = FIXTURE) => {
      await content.click();
      await page.keyboard.press("ControlOrMeta+A");
      await page.keyboard.insertText(text);
      await until(async () => (await doc()).includes("echo test"), 5000);
      // Checked a moment after the typing stops (its check is what the completions read).
      await page.waitForTimeout(1200);
    };
    // A completion: after `line`'s end, `typed` then `trigger` typed key by key; what's offered.
    const offered = async (line, typed, trigger) => {
      await load();
      await page.locator(".cm-line", { hasText: line }).first().click();
      await page.keyboard.press("End");
      await page.keyboard.insertText(typed);
      await page.keyboard.type(trigger);
      const list = page.locator(".cm-tooltip-autocomplete li");
      await until(async () => (await list.count()) > 0, 8000);
      const labels = await list.allInnerTexts();
      await page.keyboard.press("Escape");
      return labels.map((l) => l.trim());
    };
    for (const [what, line, typed, trigger, want] of [
      ["needs.", "- run: echo test", "\n        env:\n          A: ${{ ", "needs.", (l) => l.some((x) => x.startsWith("build"))],
      ["needs.<job>.outputs.", "- run: echo test", "\n        env:\n          A: ${{ needs.build.outputs", ".", (l) => l.some((x) => x.startsWith("version"))],
      ["steps.", "- run: echo build", "\n      - run: echo ${{ ", "steps.", (l) => l.some((x) => x.startsWith("v"))],
      ["matrix.", "- run: echo build", "\n      - run: echo ${{ ", "matrix.", (l) => l.some((x) => x.startsWith("os"))],
      ["secrets.", "- run: echo build", "\n      - run: echo ${{ ", "secrets.", (l) => l.some((x) => x.startsWith("GITHUB_TOKEN"))],
      ["uses:", "- run: echo build", "\n      - ", "uses: kiln/c", (l) => l.some((x) => x.startsWith("kiln/checkout@"))],
      ["with:", "- run: echo build", "\n      - uses: kiln/checkout@v1\n        with:\n          ", "fe", (l) => l.some((x) => x.startsWith("fetch-depth"))],
    ]) {
      const labels = await offered(line, typed, trigger);
      check(name, `completes ${what}`, want(labels), labels.slice(0, 8).join(", ") || "nothing offered");
    }
    // A diagnostic's fix, taken from it: the key it likely means, in place of the unknown one.
    await load(FIXTURE.replace("    runs-on: local\n    strategy", "    runs_on: local\n    strategy"));
    const mark = page.locator(".cm-lintRange-error").first();
    await until(async () => (await mark.count()) > 0, 15000);
    await mark.hover();
    const fix = page.locator(".cm-diagnosticAction", { hasText: "Rename it" }).first();
    const offeredFix = await until(async () => (await fix.count()) > 0, 8000);
    if (offeredFix) await fix.click();
    check(name, "a diagnostic's fix, taken from it", offeredFix && (await until(async () => !(await doc()).includes("runs_on") && (await doc()).includes("  build:   # first\n    runs-on: local"), 5000)), (await doc()).slice(0, 160));
    // Through the flow view and back, nothing touched: the same text, its comments with it.
    await load();
    const view = page.getByRole("group", { name: "View" });
    await view.getByRole("button", { name: "Flow", exact: true }).click();
    await page.getByRole("region", { name: "Its flow" }).waitFor({ timeout: 15000 });
    await view.getByRole("button", { name: "YAML", exact: true }).click();
    await content.waitFor({ timeout: 15000 });
    const back = await doc();
    check(name, "through the flow view and back: the same text", back.trimEnd() === FIXTURE.trimEnd(), back.slice(0, 200));
    // And with an edit made there: just that, its comments kept.
    await view.getByRole("button", { name: "Flow", exact: true }).click();
    const flow = page.getByRole("region", { name: "Its flow" });
    await flow.getByRole("region", { name: "Job test" }).getByLabel("Add a label").selectOption("native");
    await view.getByRole("button", { name: "YAML", exact: true }).click();
    await content.waitFor({ timeout: 15000 });
    const edited = await doc();
    const want = FIXTURE.replace("  test:\n    needs: build\n    runs-on: local\n", "  test:\n    needs: build\n    runs-on: [local, native]\n");
    check(name, "an edit there, and only it, its comments kept", edited.trimEnd() === want.trimEnd(), edited.slice(0, 400));
    await checkLayout(check, `${name}: wide: the editor`, page);
    await page.screenshot({ path: `${shots}/editor-${name}.png`, fullPage: true });
    check(name, "no page errors", errors.length === 0, errors.join("; "));
    await ctx.close();
  } finally {
    await browser.close();
  }
}

console.log(report.join("\n"));
process.exit(failed ? 1 : 0);
