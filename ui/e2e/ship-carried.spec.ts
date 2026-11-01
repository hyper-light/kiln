/**
 * An approval carried to a rebase-only version says so under Ship's Approve row, in kiln's words
 * (docs/ci.md W27-R08: kiln signs the carry as a transfer, from the version it was given on to the
 * one it reached). The mock (mock/review.ts) answers D41's verdicts: lee's approval, given on v1,
 * carried to v2. Chromium and WebKit, desktop and phone.
 */
import { expect, test } from "@playwright/test";

test("an approval carried from v1 says so under Approve, there as the row is, within the row's width", async ({ page }) => {
  await page.goto("/ship");
  const row = page.locator(".ship-row", { hasText: "pipeline: dynamic matrices" }).first();
  await row.locator(".ship-main").click();
  const bar = row.locator(".rv-bar").first();
  await expect(bar).toBeVisible();
  // Read with the row's other data: there as the row is, never arriving after it.
  const line = row.locator(".rv-carried");
  expect(await line.count()).toBe(1);
  await expect(line).toHaveText(/^lee's approval carried from v1: rebase only, patch-id 9f3c41a…, base [0-9a-f]{7} to [0-9a-f]{7}$/);
  // Under the Approve row, inside the Files section: nothing of it outside, at every width.
  const [b, l, files] = await Promise.all([bar.boundingBox(), line.boundingBox(), row.getByRole("region", { name: "Files", exact: true }).boundingBox()]);
  expect(b && l && files).toBeTruthy();
  if (b && l && files) {
    expect(l.y).toBeGreaterThanOrEqual(b.y + b.height - 1);
    expect(l.x).toBeGreaterThanOrEqual(files.x - 1);
    expect(l.x + l.width).toBeLessThanOrEqual(files.x + files.width + 1);
  }
});
