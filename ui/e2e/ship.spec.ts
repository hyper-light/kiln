/**
 * Ship (docs/ci.md W26-R01, R02), in the production build against the mock's commits: D41 sits
 * in Ship's list as Needs you · kim: empty matrix meant? · 1 thread, first, with Ship's Hand off
 * and Review pair; the change request's other commit folds under its head; the Review lens and the
 * host facet narrow the list; an item opens to Files, Output, Jobs and Lanes, in that order.
 */
import { expect, test } from "@playwright/test";

const REAL = !!process.env.E2E_BASE_URL;
test.skip(REAL, "drives the mock's commits");

test("changes are Ship items, drawn by Ship's own row, needs you first", async ({ page }) => {
  await page.goto("/ship");
  const list = page.getByRole("list", { name: "Commits", exact: true });
  const rows = list.locator(":scope > li");
  await expect(rows.first()).toContainText("pipeline: dynamic matrices");
  await expect(rows.first()).toContainText("Needs you");
  await expect(rows.first()).toContainText("kim: empty matrix meant? · 1 thread");
  await expect(rows.first()).toContainText("D41 · requested");
  await expect(rows.first().getByRole("button", { name: "Hand off" })).toBeVisible();
  await expect(rows.first().getByRole("button", { name: "Review" })).toBeVisible();

  // The change request's other commit, folded under its head.
  const cr = rows.filter({ hasText: "sched: fair shares" });
  await cr.getByRole("button", { name: /Show 1 more commit of CR12/ }).click();
  await expect(cr.getByRole("list", { name: "The other commits of CR12" })).toContainText("sched: weights");

  // Review opens the item at its files; the dropdown's order.
  await rows.first().getByRole("button", { name: "Review" }).click();
  const sections = rows.first().locator(".ship-sec h3");
  await expect(sections).toHaveText(["Files", "Output", "Jobs", "Lanes"]);

  // The Review lens: changes only. The host facet: GitHub's.
  await page.getByRole("group", { name: "Lens" }).getByRole("button", { name: "Review" }).click();
  await expect(page).toHaveURL(/lens=review/);
  await expect(list).not.toContainText("deploy: a rollback holds");
  await page.goto("/ship?host=github");
  await expect(page.getByRole("list", { name: "Commits", exact: true }).locator(":scope > li")).toHaveCount(1);
  await expect(page.getByRole("list", { name: "Commits", exact: true })).toContainText("Faster builds");
});
