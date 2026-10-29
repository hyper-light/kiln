/**
 * Ship's list bar (plan 01a175b7f20b7277b15bddd2ad001bcc, step 4), against the mock's fixed rows:
 * a repository chosen from the filter narrows the list and hides the placeholder; the sort reorders
 * it (oldest first, newest first).
 */
import { expect, test } from "@playwright/test";

const REAL = !!process.env.E2E_BASE_URL;
test.skip(REAL, "drives the mock's commits");

test("choose a repository, change the sort: the list follows", async ({ page }) => {
  await page.goto("/ship");
  const rows = page.getByRole("list", { name: "Commits", exact: true }).locator(":scope > li");
  // A first load on a busy machine can take a while (as the other Ship specs allow).
  await expect(rows.first()).toBeVisible({ timeout: 15_000 });

  const box = page.getByRole("combobox", { name: "Filter commits" });
  await box.click();
  await box.fill("hyper-light/kiln");
  await page.getByRole("option", { name: "hyper-light/kiln", exact: true }).first().click();
  await expect(page).toHaveURL(/[?&]repo=hyper-light(%2F|\/)kiln/);
  await expect(box).toHaveAttribute("placeholder", "");
  await expect(rows.first()).toBeVisible();

  const sort = page.getByRole("combobox", { name: "Sort commits" });
  await sort.selectOption("oldest");
  await expect(page).toHaveURL(/[?&]sort=oldest/);
  await expect(rows.first()).toContainText("deploy: a rollback holds");
  await sort.selectOption("newest");
  await expect(rows.first()).toContainText("pipeline: dynamic matrices");
});
