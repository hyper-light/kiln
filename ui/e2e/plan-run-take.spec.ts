/**
 * A plan run's step taken over (docs/ci.md W27-R08), against the mock: the page says in place how
 * the take-over is signed, and signs it. A browser that can sign (WebCrypto, on a secure page: the
 * mock's 127.0.0.1 is one) signs the person's receipt with its own key, certified as them; one that
 * can't has kiln sign it for them, the page saying why and how to sign it themselves (`kiln login`,
 * then `kiln plans take <run> <n>`). A line beside the step's actions, there with Take it over,
 * never a popup, and the page keeps to its width.
 */
import { expect, test, type Page } from "@playwright/test";

const REAL = !!process.env.E2E_BASE_URL;
test.skip(REAL, "drives the mock's plan run");

test.beforeEach(async ({ request }) => {
  expect((await request.post("/api/mock/plans/reset")).ok()).toBe(true);
});

const takeable = (page: Page) => page.locator("li.run-step", { has: page.getByRole("button", { name: "Take it over" }) }).first();

const receipts = async (page: Page) =>
  (
    (await (await page.request.get("/api/mock/plans/receipts")).json()) as {
      receipts: { receipt?: { dsseEnvelope?: { payloadType?: string; signatures?: { sig?: string }[] }; verificationMaterial?: { certificate?: { rawBytes?: string } } }; unsigned?: string }[];
    }
  ).receipts;

test("a step to take over says, in place, that this browser signs it", async ({ page }) => {
  await page.goto("/agents/plans/runs/pr_agents1");
  const step = takeable(page);
  await expect(step).toBeVisible();
  await expect(step).toContainText("Taken over here, you sign your receipt of it in this browser, with its own key.");
  const overflow = await page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);
  expect(overflow).toBeLessThanOrEqual(0);
});

test("taking a step over signs its receipt in this browser, with its own key", async ({ page }) => {
  await page.goto("/agents/plans/runs/pr_agents1");
  const step = takeable(page);
  await expect(step).toBeVisible();
  const sent = page.waitForRequest((r) => r.url().endsWith("/receipt") && r.method() === "POST");
  await step.getByRole("button", { name: "Take it over" }).click();
  await sent;
  await expect.poll(async () => (await receipts(page)).length).toBe(1);
  const [r] = await receipts(page);
  expect(r?.unsigned).toBeUndefined();
  expect(r?.receipt?.dsseEnvelope?.payloadType).toBe("application/vnd.kiln.custody+json");
  expect(r?.receipt?.dsseEnvelope?.signatures?.[0]?.sig?.length ?? 0).toBeGreaterThan(0);
  expect(r?.receipt?.verificationMaterial?.certificate?.rawBytes?.length ?? 0).toBeGreaterThan(0);
});

test("a browser that can't sign says kiln signs it for them, and why", async ({ page }) => {
  // A page that isn't secure has no WebCrypto: as here.
  await page.addInitScript(() => Object.defineProperty(globalThis.crypto, "subtle", { value: undefined }));
  await page.goto("/agents/plans/runs/pr_agents1");
  const step = takeable(page);
  await expect(step).toBeVisible();
  const n = (await step.locator(".run-n").innerText()).trim();
  await expect(step).toContainText("Taken over here, kiln signs your receipt of it for you: this page isn't secure (https), so this browser has no key.");
  await expect(step.locator("code", { hasText: `kiln plans take pr_agents1 ${n}` })).toBeVisible();
  await expect(step.locator("code", { hasText: "kiln login" })).toBeVisible();
  const taken = page.waitForResponse((r) => r.url().endsWith("/take") && r.request().method() === "POST");
  await step.getByRole("button", { name: "Take it over" }).click();
  expect(((await (await taken).json()) as { custody?: { kiln_signed_for_you?: string | null } }).custody?.kiln_signed_for_you).toBeTruthy();
  expect(await receipts(page)).toEqual([]);
});
