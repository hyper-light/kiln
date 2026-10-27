/**
 * The opened commit's Output (plan 01a17ba155a17737a39348c6c5362210, step 3), against the mock: the
 * first row's picked job (Checks, streaming one of the mock's own jobs) shows in a card exactly the
 * lanes card's width at 1280, 1000, 800, 600 and 390; as the job prints, nothing moves; scrolled up,
 * the log stays where it was.
 */
import { expect, test, type Locator } from "@playwright/test";

const REAL = !!process.env.E2E_BASE_URL;
test.skip(REAL, "drives the mock's commits");

/** The Output card's width, the lanes card's inner width, and where the card sits. */
const measure = (row: Locator) =>
  row.evaluate((li) => {
    const out = li.querySelector(".ship-output");
    const lanes = li.querySelector('section[aria-label="Lanes"]');
    if (!out || !lanes) return null;
    const cs = getComputedStyle(lanes);
    const inner = lanes.getBoundingClientRect().width - parseFloat(cs.paddingLeft) - parseFloat(cs.paddingRight) - parseFloat(cs.borderLeftWidth) - parseFloat(cs.borderRightWidth);
    const r = out.getBoundingClientRect();
    return { width: r.width, lanes: inner, top: Math.round(r.top), height: Math.round(r.height) };
  });

for (const width of [1280, 1000, 800, 600, 390]) {
  test(`at ${width}, the output is the lanes card's width and holds still as the job prints`, async ({ page, isMobile, browserName }) => {
    test.skip(isMobile, "each width is set here; the desktop projects run them all");
    await page.setViewportSize({ width, height: 900 });
    await page.goto("/ship");
    const row = page.getByRole("list", { name: "Commits", exact: true }).locator(":scope > li").first();
    await row.getByRole("button", { name: /^Open / }).click();
    await expect(row.locator(".ship-output .xterm")).toBeVisible({ timeout: 15_000 });
    const first = await measure(row);
    expect(first).not.toBeNull();
    expect(Math.abs((first?.width ?? 0) - (first?.lanes ?? 0))).toBeLessThanOrEqual(1);

    if (browserName === "chromium")
      await page.evaluate(() => {
        const w = window as unknown as { __shift: number };
        w.__shift = 0;
        new PerformanceObserver((l) => {
          for (const e of l.getEntries() as unknown as { value: number; hadRecentInput: boolean }[]) if (!e.hadRecentInput) w.__shift += e.value;
        }).observe({ type: "layout-shift" });
      });
    await page.waitForTimeout(3000);
    const after = await measure(row);
    expect(after?.top).toBe(first?.top);
    expect(after?.height).toBe(first?.height);
    if (browserName === "chromium") expect(await page.evaluate(() => (window as unknown as { __shift: number }).__shift)).toBe(0);
  });
}

test("scrolled up, the output stays where you are", async ({ page, isMobile }) => {
  test.skip(isMobile, "the desktop projects run it");
  await page.goto("/ship");
  const row = page.getByRole("list", { name: "Commits", exact: true }).locator(":scope > li").first();
  await row.getByRole("button", { name: /^Open / }).click();
  const viewport = row.locator(".ship-output .xterm-viewport");
  await expect(viewport).toBeAttached({ timeout: 15_000 });
  await viewport.evaluate((v) => v.scrollTo({ top: 0 }));
  await viewport.dispatchEvent("wheel", { deltaY: -400 });
  await page.waitForTimeout(2500);
  expect(await viewport.evaluate((v) => v.scrollTop)).toBe(0);
});
