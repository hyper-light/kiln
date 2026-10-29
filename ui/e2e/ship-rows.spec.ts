/**
 * Ship's rows (plan 01a175b7f20b7277b15bddd2ad001bcc, step 5), at 390 against the mock's fixed rows:
 * every row keeps one height whatever its state, and keeps it as a row opens, a change unfolds and
 * the row closes again.
 */
import { expect, test, type Page } from "@playwright/test";

const REAL = !!process.env.E2E_BASE_URL;
test.skip(REAL, "drives the mock's commits");
test.use({ viewport: { width: 390, height: 844 } });

/** Each row's own line (not what opens or unfolds under it), in CSS pixels. */
const heights = (page: Page) => page.locator(".ship-row > .ship-r").evaluateAll((els) => els.map((e) => Math.round(e.getBoundingClientRect().height)));

test("a row keeps one height whatever its state", async ({ page }) => {
  await page.goto("/ship");
  const rows = page.getByRole("list", { name: "Commits", exact: true }).locator(":scope > li");
  await expect(rows.first()).toBeVisible();
  const before = await heights(page);
  expect(before.length).toBeGreaterThan(3);
  expect(new Set(before).size).toBe(1);

  await rows
    .first()
    .getByRole("button", { name: /^Open / })
    .click();
  await page.getByRole("button", { name: /Show 1 more commit of CR12/ }).click();
  const during = await heights(page);
  expect(new Set(during).size).toBe(1);
  expect(during[0]).toBe(before[0]);

  await rows
    .first()
    .getByRole("button", { name: /^Close / })
    .click();
  expect(await heights(page)).toEqual(before);
});
