// W10's sign-in in Chromium and WebKit, wide and on phones. In Chromium, with a virtual
// authenticator: the sign-in page offers GitHub before anything else; lead makes a passkey and a
// password of their own in Settings → Sign-in; then the sign-in page asks for a name and offers
// the passkey, and both sign lead in. WebKit and phones: the same pages laid out, nothing over.
import { chromium, webkit, devices } from "/pw/node_modules/playwright-core/index.mjs";
import { checkLayout, pageErrors } from "./layout.mjs";

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
const whoAmI = (page) => page.evaluate(() => fetch("/api/me").then((r) => r.json()).then((m) => m.principal ?? null));
async function linkIn(ctx, who) {
  const r = await ctx.request.post(`${cfg.base}/api/users/${who}/link`, { headers: { Authorization: `Bearer ${cfg.admin}` } });
  if (!r.ok()) throw new Error(`link: ${r.status()} ${await r.text()}`);
  const { url } = await r.json();
  const p = await ctx.newPage();
  await p.goto(url);
  await p.close();
}
async function signInPage(page) {
  await page.goto(`${cfg.base}/`);
  await page.getByRole("button", { name: "Sign in", exact: true }).waitFor({ timeout: 30000 });
  await page.waitForTimeout(500);
}

const flow = async (label, f) => {
  try {
    await f();
  } catch (e) {
    check(label, "ran to its end", false, String(e).split("\n")[0]);
  }
};

// Chromium, wide: the ceremonies.
await flow("chromium wide", async () => {
  const name = "chromium";
  const browser = await chromium.launch();
  try {
    const ctx = await browser.newContext({ viewport: { width: 1440, height: 900 } });
    const page = await ctx.newPage();
    const errors = pageErrors(page, cfg.base);
    const cdp = await ctx.newCDPSession(page);
    await cdp.send("WebAuthn.enable");
    await cdp.send("WebAuthn.addVirtualAuthenticator", { options: { protocol: "ctap2", transport: "internal", hasResidentKey: true, hasUserVerification: true, isUserVerified: true, automaticPresenceSimulation: true } });

    await signInPage(page);
    const github = page.getByRole("link", { name: "Continue with GitHub" });
    check(name, "wide: GitHub's button, to its start", (await github.count()) === 1 && (await github.getAttribute("href")) === "/api/auth/github/start");
    check(name, "wide: no passkey yet, so no passkey button", (await page.getByRole("button", { name: "Sign in with a passkey" }).count()) === 0);
    check(name, "wide: no person's password yet, so no name", (await page.locator("#who").count()) === 0);
    await page.screenshot({ path: `${shots}/signin-${name}-wide-first.png`, fullPage: true });

    // lead, signed in by link: a passkey and a password of their own.
    await linkIn(ctx, "lead");
    await page.goto(`${cfg.base}/settings#signin.passkeys`);
    await page.locator("#st-passkey-name").waitFor({ timeout: 30000 });
    await page.locator("#st-passkey-name").fill("Laptop");
    await page.getByRole("button", { name: "Make a passkey" }).click();
    const keys = page.getByRole("list", { name: "Passkeys" });
    check(name, "wide: the passkey is made and listed", await until(async () => /Laptop/.test(await keys.innerText()) && /never used/.test(await keys.innerText())), await page.locator("[id='signin.passkeys']").innerText().catch(() => ""));
    await page.locator("#st-my-new").fill("correct horse battery");
    await page.getByRole("button", { name: "Set it" }).click();
    check(name, "wide: lead's password is set", await until(async () => /Set: sign in with your name and it/.test(await page.locator("[id='signin.mine']").innerText())));
    check(name, "wide: Settings fits", (await overflow(page)) <= 0, String(await overflow(page)));
    await checkLayout(check, `${name}: wide: Settings`, page);
    await page.screenshot({ path: `${shots}/signin-${name}-wide-settings.png`, fullPage: true });

    // Signed out: a name, and the passkey, offered; the passkey signs lead in.
    await ctx.clearCookies();
    await signInPage(page);
    check(name, "wide: the name is asked", await page.locator("#who").isVisible());
    const byKey = page.getByRole("button", { name: "Sign in with a passkey" });
    check(name, "wide: the passkey is offered", (await byKey.count()) === 1);
    check(name, "wide: the sign-in page fits", (await overflow(page)) <= 0, String(await overflow(page)));
    await checkLayout(check, `${name}: wide: the sign-in page`, page);
    await page.screenshot({ path: `${shots}/signin-${name}-wide-offered.png`, fullPage: true });
    await byKey.click();
    check(name, "wide: the passkey signs lead in", await until(async () => (await whoAmI(page)) === "user:lead"), String(await whoAmI(page)));

    // Signed out again: lead's name and password; a wrong one said so.
    await ctx.clearCookies();
    await signInPage(page);
    await page.locator("#who").fill("lead");
    await page.locator("#secret").fill("wrong horse battery");
    await page.getByRole("button", { name: "Sign in", exact: true }).click();
    check(name, "wide: a wrong password said so", await until(async () => /Wrong name or password/.test(await page.locator("main").innerText())));
    await page.locator("#secret").fill("correct horse battery");
    await page.getByRole("button", { name: "Sign in", exact: true }).click();
    check(name, "wide: name and password sign lead in", await until(async () => (await whoAmI(page)) === "user:lead"), String(await whoAmI(page)));
    check(name, "wide: no page errors", errors.length === 0, errors.join("; "));
    await ctx.close();
  } finally {
    await browser.close();
  }
});

// Both browsers, wide and on phones: the pages laid out (WebKit's ceremonies aren't scriptable).
for (const [name, type, phone] of [
  ["chromium", chromium, { ...devices["Pixel 7"], viewport: { width: 390, height: 844 } }],
  ["webkit", webkit, devices["iPhone 13"]],
]) await flow(name, async () => {
  const browser = await type.launch();
  try {
    for (const [tag, options] of [
      ["wide", { viewport: { width: 1440, height: 900 } }],
      ["phone", phone],
    ]) {
      if (name === "chromium" && tag === "wide") continue;
      const ctx = await browser.newContext(options);
      const page = await ctx.newPage();
      const errors = pageErrors(page, cfg.base);
      await signInPage(page);
      check(name, `${tag}: the name, the passkey and GitHub offered`, (await page.locator("#who").isVisible()) && (await page.getByRole("button", { name: "Sign in with a passkey" }).count()) === 1 && (await page.getByRole("link", { name: "Continue with GitHub" }).count()) === 1);
      check(name, `${tag}: the sign-in page fits`, (await overflow(page)) <= 0, String(await overflow(page)));
      await checkLayout(check, `${name}: ${tag}: the sign-in page`, page);
      await page.screenshot({ path: `${shots}/signin-${name}-${tag}-page.png`, fullPage: true });
      await linkIn(ctx, "lead");
      await page.goto(`${cfg.base}/settings#signin.passkeys`);
      const keys = page.getByRole("list", { name: "Passkeys" });
      check(name, `${tag}: lead's passkey listed`, await until(async () => /Laptop/.test(await keys.innerText()), 30000));
      check(name, `${tag}: lead's password set`, await until(async () => /Set/.test(await page.locator("[id='signin.mine']").innerText())));
      check(name, `${tag}: Settings fits`, (await overflow(page)) <= 0, String(await overflow(page)));
      await checkLayout(check, `${name}: ${tag}: Settings`, page);
      await page.screenshot({ path: `${shots}/signin-${name}-${tag}-settings.png`, fullPage: true });
      check(name, `${tag}: no page errors`, errors.length === 0, errors.join("; "));
      await ctx.close();
    }
  } finally {
    await browser.close();
  }
});

console.log(report.join("\n"));
process.exit(failed ? 1 : 0);
