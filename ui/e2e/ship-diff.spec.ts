/**
 * The Diff button (Changes plan 01a17bd748ff73d98f09996ecdc6873d, step 4), against the mock's fixed
 * rows. An item's diff lives in one place, Review's Files first in its dropdown (Ada, 2026-10-27):
 * a change's Review opens its row in place and brings that Files into view, focused, without leaving
 * /ship; opening a row any other way leaves the page where it is.
 */
import { expect, test } from "@playwright/test";

const REAL = !!process.env.E2E_BASE_URL;
test.skip(REAL, "drives the mock's commits");

test("a change's Review opens its row at its one Files section, in view and focused, on /ship", async ({ page }) => {
  await page.goto("/ship");
  const row = page.locator(".ship-row").filter({ hasText: "D41" }).first();
  await expect(row).toBeVisible({ timeout: 15_000 });
  await row.getByRole("button", { name: "Review", exact: true }).click();
  await expect(row.locator(".ship-go")).toHaveAttribute("aria-expanded", "true");
  const files = row.locator('section[aria-label="Files"]');
  await expect(files).toHaveCount(1);
  await expect(files).toBeInViewport();
  await expect(files).toBeFocused();
  expect(new URL(page.url()).pathname).toBe("/ship");
});

test("with its review slow to load, Review still ends at its loaded Files, focused", async ({ page }) => {
  // Review's data held back a moment, so Files comes late (after Review's placeholder, where it draws one).
  await page.route("**/api/diffs/**", async (route) => {
    await new Promise((done) => setTimeout(done, 1500));
    await route.continue();
  });
  await page.goto("/ship");
  const row = page.locator(".ship-row").filter({ hasText: "D41" }).first();
  await expect(row).toBeVisible({ timeout: 15_000 });
  await row.getByRole("button", { name: "Review", exact: true }).click();
  const files = row.locator('section[aria-label="Files"]');
  // Loaded: Review's summary is in its heading.
  await expect(files.locator(".rv-sum")).toBeVisible({ timeout: 15_000 });
  await expect(files).toHaveCount(1);
  await expect(files).toBeFocused();
  await expect(files).toBeInViewport();
});

test("opening a row by its chevron leaves the page where it is", async ({ page }) => {
  await page.goto("/ship");
  const row = page.locator(".ship-row").filter({ hasText: "D41" }).first();
  await expect(row).toBeVisible({ timeout: 15_000 });
  // The chevron in view, and the pointer resting on it, so the list holds still (a commit another test
  // adds above can't move it), then a click where it is: what's measured is the app's own move, not a
  // click's scroll to a chevron that moved.
  const chevron = row.locator(".ship-go");
  await chevron.scrollIntoViewIfNeeded();
  const box = await chevron.boundingBox();
  if (!box) throw new Error("no chevron");
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
  const before = await page.evaluate(() => window.scrollY);
  const at = (await chevron.boundingBox()) ?? box;
  await page.mouse.click(at.x + at.width / 2, at.y + at.height / 2);
  await expect(row.locator(".ship-go")).toHaveAttribute("aria-expanded", "true");
  const files = row.locator('section[aria-label="Files"]');
  await expect(files).toHaveCount(1);
  await expect(files).not.toBeFocused();
  expect(await page.evaluate(() => window.scrollY)).toBe(before);
});
