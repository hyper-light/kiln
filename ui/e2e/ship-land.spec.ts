/**
 * Land beside Approve in a Ship item's gate row (docs/ci.md W26-R19): while Review's gate holds a
 * change, Land when ready has Review hand it to the queue once it clears, and then it says so,
 * disabled. Land keeps its place and width whatever it says, inside the row. The mock
 * (mock/review.ts) holds D41 on kim's request for changes. Chromium and WebKit, desktop and phone.
 */
import { expect, test } from "@playwright/test";

test("a held change asks to land when ready, then says so, in its place", async ({ page }) => {
  // Each project starts from D41 not yet asked to land (the mock is shared by every project).
  const unask = () => page.request.post("/api/diffs/D41/land", { data: { when_ready: false } });
  await unask();
  await page.goto("/ship");
  const row = page.locator(".ship-row", { hasText: "pipeline: dynamic matrices" }).first();
  await row.locator(".ship-main").click();
  const bar = row.locator(".rv-bar").first();
  const land = bar.locator(".rv-land");
  await expect(land).toHaveText("Land when ready");
  await expect(land).toHaveAttribute("title", /kim requested changes/);
  await expect(land).toBeEnabled();
  const before = await land.boundingBox();
  await land.click();
  await expect(land).toHaveText("Lands when ready");
  await expect(land).toBeDisabled();
  const [after, b] = await Promise.all([land.boundingBox(), bar.boundingBox()]);
  expect(before && after && b).toBeTruthy();
  if (before && after && b) {
    expect(Math.abs(after.x - before.x)).toBeLessThanOrEqual(1);
    expect(Math.abs(after.width - before.width)).toBeLessThanOrEqual(1);
    expect(after.x + after.width).toBeLessThanOrEqual(b.x + b.width + 1);
  }
  await unask();
});
