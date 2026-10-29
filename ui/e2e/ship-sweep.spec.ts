/**
 * An opened commit never overflows sideways (plan 01a175b7f21b7455bcdcd71e94ecdce0, step 4), against
 * the mock's fixed rows: from 320 to 1440 px, with D41 open (Files, the Conversation, Output, Jobs and
 * Lanes drawn), and its lanes in Fit and in Detail, the document is exactly as wide as its window.
 */
import { expect, test, type Page } from "@playwright/test";

const REAL = !!process.env.E2E_BASE_URL;
test.skip(REAL, "drives the mock's commits");

/** 320 to 1440: phones, tablets and desktops, and the job cards' and lanes' own breakpoints either side. */
const WIDTHS = [320, 360, 390, 414, 440, 520, 600, 640, 700, 768, 820, 900, 1000, 1024, 1100, 1180, 1280, 1366, 1440];

/** The document's width against its window's. */
const widths = (page: Page) => page.evaluate(() => ({ scroll: document.documentElement.scrollWidth, client: document.documentElement.clientWidth }));

test("from 320 to 1440 px an opened commit is never wider than its window, its lanes in Fit or Detail", async ({ page, isMobile }) => {
  test.skip(isMobile, "the widths are set here; the desktop projects run them all");
  test.setTimeout(240_000);
  await page.setViewportSize({ width: 1440, height: 900 });
  await page.goto("/ship");
  const row = page.locator(".ship-row").filter({ hasText: "D41" }).first();
  await expect(row).toBeVisible({ timeout: 15_000 });
  await row.locator(".ship-go").click();
  await expect(row.locator(".rv-s > h3, .ship-sech > h3")).toHaveText([/^Files/, "Conversation", "Output", "Jobs", "Lanes"], { timeout: 15_000 });
  await expect(row.locator(".ship-lanesbox")).toBeVisible({ timeout: 15_000 });
  const scale = row.locator('section[aria-label="Lanes"]').getByRole("group", { name: "Lanes' scale" });
  const wide: string[] = [];
  for (const width of WIDTHS) {
    await page.setViewportSize({ width, height: 900 });
    for (const room of ["Fit", "Detail"]) {
      // Fit and Detail, side by side in the Lanes' head (f-ship's): the one pressed is the one shown.
      await scale.getByRole("button", { name: room }).click();
      await expect(scale.getByRole("button", { name: room })).toHaveAttribute("aria-pressed", "true");
      await page.waitForTimeout(100);
      const w = await widths(page);
      if (w.scroll !== w.client) wide.push(`${width} px, ${room}: ${w.scroll} wide in ${w.client}`);
    }
  }
  expect(wide).toEqual([]);
});
