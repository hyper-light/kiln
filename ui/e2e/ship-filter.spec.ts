/**
 * Ship's list bar (plan 01a175b7f20b7277b15bddd2ad001bcc, step 4), against the mock's fixed rows,
 * whose rows are repositories (docs/ci.md W26-R29): the sort reorders them by their newest commits
 * (oldest first, newest first); a repository chosen from the filter narrows the list to its row and
 * hides the placeholder.
 */
import { expect, test } from "@playwright/test";

const REAL = !!process.env.E2E_BASE_URL;
test.skip(REAL, "drives the mock's commits");

test("change the sort, choose a repository: the list follows", async ({ page }) => {
  await page.goto("/ship");
  const rows = page.getByRole("list", { name: "Repositories", exact: true }).locator(":scope > li");
  // A first load on a busy machine can take a while (as the other Ship specs allow).
  await expect(rows.first()).toBeVisible({ timeout: 15_000 });
  const names = rows.locator(":scope > .ship-r .ship-name");

  // By their newest commits: kiln's 10 minutes ago, acme/web's 30, hyper-raft's 45.
  const sort = page.getByRole("combobox", { name: "Sort commits" });
  await sort.selectOption("oldest");
  await expect(page).toHaveURL(/[?&]sort=oldest/);
  await expect(names).toHaveText(["hyper-light/hyper-raft", "acme/web", "hyper-light/kiln"]);
  await sort.selectOption("newest");
  await expect(names).toHaveText(["hyper-light/kiln", "acme/web", "hyper-light/hyper-raft"]);

  const box = page.getByRole("combobox", { name: "Filter commits" });
  await box.click();
  await box.fill("hyper-light/kiln");
  await page.getByRole("option", { name: "hyper-light/kiln", exact: true }).first().click();
  await expect(page).toHaveURL(/[?&]repo=hyper-light(%2F|\/)kiln/);
  await expect(box).toHaveAttribute("placeholder", "");
  await expect(names).toHaveText(["hyper-light/kiln"]);
});
