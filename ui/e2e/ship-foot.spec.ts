/**
 * An opened commit's foot as f-ship draws it (plan 01a1856fe81375b1bd1d1d86f270a45b, step 11),
 * against the mock's D41: Runs, Diff and Commit, each going where it says: Runs to the commit's
 * runs on its page, Diff to its Files here, Commit to its page.
 */
import { expect, test, type Page } from "@playwright/test";

const REAL = !!process.env.E2E_BASE_URL;
test.skip(REAL, "drives the mock's commits");

const openD41 = async (page: Page) => {
  await page.goto("/ship");
  const row = page.locator(".ship-row").filter({ hasText: "D41" }).first();
  await expect(row).toBeVisible({ timeout: 15_000 });
  await row.locator(".ship-go").click();
  const foot = row.getByRole("navigation", { name: /'s pages$/ });
  await expect(foot).toBeVisible({ timeout: 15_000 });
  return { row, foot };
};

test("the foot is Runs, Diff and Commit, each going where it says", async ({ page }) => {
  test.setTimeout(90_000);
  const { row, foot } = await openD41(page);
  await expect(foot.locator(":scope > *")).toHaveText(["Runs", "Diff", "Commit"]);
  // From the left, as Ada asked (2026-11-01): the first where the foot starts.
  const [footAt, firstAt] = await Promise.all([foot.boundingBox(), foot.locator(":scope > *").first().boundingBox()]);
  expect(Math.abs((firstAt?.x ?? Infinity) - (footAt?.x ?? 0))).toBeLessThanOrEqual(1);
  // Diff: its Files here, brought into view and focused.
  await page.evaluate(() => window.scrollTo(0, document.documentElement.scrollHeight));
  await foot.getByRole("button", { name: "Diff" }).click();
  const files = row.locator('section[aria-label="Files"]');
  await expect(files).toBeFocused({ timeout: 15_000 });
  await expect(files).toBeInViewport();
  // Runs: the commit's runs, on its page.
  await foot.getByRole("link", { name: "Runs" }).click();
  await expect(page).toHaveURL(/\/ship\/hyper-light\/kiln\/[0-9a-f]{7,40}#runs$/);
  await expect(page.getByRole("region", { name: "Runs" })).toBeVisible({ timeout: 15_000 });
  // Commit: its page.
  const again = await openD41(page);
  await again.foot.getByRole("link", { name: "Commit", exact: true }).click();
  await expect(page).toHaveURL(/\/ship\/hyper-light\/kiln\/[0-9a-f]{7,40}$/);
  await expect(page.getByRole("region", { name: "Commit" })).toBeVisible({ timeout: 15_000 });
});
