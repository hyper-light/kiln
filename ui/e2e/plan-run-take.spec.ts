/**
 * A plan run's step taken over (docs/ci.md W27-R08), against the mock: the page says in place how
 * the take-over is signed. From this browser, which has no key, kiln signs the person's receipt for
 * them; `kiln login`, then `kiln plans take <run> <n>`, signs it on their device. A line beside the
 * step's actions, there with Take it over, never a popup, and the page keeps to its width.
 */
import { expect, test } from "@playwright/test";

const REAL = !!process.env.E2E_BASE_URL;
test.skip(REAL, "drives the mock's plan run");

test("a step to take over says, in place, how taking it over is signed", async ({ page }) => {
  await page.goto("/agents/plans/runs/pr_agents1");
  const step = page.locator("li.run-step", { has: page.getByRole("button", { name: "Take it over" }) }).first();
  await expect(step).toBeVisible();
  const n = (await step.locator(".run-n").innerText()).trim();
  await expect(step).toContainText("Taken over here, kiln signs your receipt of it for you: this browser has no key.");
  await expect(step.locator("code", { hasText: `kiln plans take pr_agents1 ${n}` })).toBeVisible();
  await expect(step.locator("code", { hasText: "kiln login" })).toBeVisible();
  const overflow = await page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);
  expect(overflow).toBeLessThanOrEqual(0);
});
