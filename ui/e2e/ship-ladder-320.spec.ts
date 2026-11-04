/** The conflict ladder (docs/ci.md W26-R17) at the narrowest phone, 320: nothing scrolls sideways, and the buttons sit in rows whose height holds as the rung line changes. */
import { expect, test } from "@playwright/test";

test.use({ viewport: { width: 320, height: 720 }, isMobile: true, hasTouch: true });
test("the ladder's conflict at 320: no sideways scroll, buttons in fixed rows", async ({ page }) => {
  await page.request.post("/api/mock/review", { data: { conflicts: true, ladder: "held", ladder_tests: "pending" } });
  await page.goto("/ship");
  const row = page.locator(".ship-row", { hasText: "pipeline: dynamic matrices" }).first();
  await row.locator(".ship-main").click();
  const cf = row.locator(".rv-cf").first();
  await cf.waitFor();
  for (const c of await row.locator(".rv-cf").all()) if ((await c.getAttribute("data-done")) === "1") await c.getByRole("button", { name: "Undo" }).click();
  await cf.scrollIntoViewIfNeeded();
  const sideways = await page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);
  expect(sideways).toBe(0);
  const grid = cf.locator(".rv-cfb");
  const before = await grid.boundingBox();
  const box = await cf.boundingBox();
  for (const b of await grid.getByRole("button").all()) {
    const bb = await b.boundingBox();
    expect(bb && box && bb.x + bb.width <= box.x + box.width + 0.5).toBeTruthy();
  }
  await page.request.post("/api/mock/review", { data: { ladder_tests: "passed", emit: { type: "conflict", diff: "D41" } } });
  await expect(cf.locator(".rv-rung")).toContainText("passed");
  expect((await grid.boundingBox())?.height).toBe(before?.height);
});
