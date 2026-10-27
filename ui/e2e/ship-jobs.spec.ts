/**
 * An opened commit's job cards (plan 01a175b7f21b7455bcdcd71e94ecdce0, step 1), against the mock's
 * fixed rows: at 1280, 1000, 800, 600 and 390 the visible cards and their gaps span exactly the
 * lanes card's width (within 1 px), and previous is off at the start.
 */
import { expect, test } from "@playwright/test";

const REAL = !!process.env.E2E_BASE_URL;
test.skip(REAL, "drives the mock's commits");

// As many to a view as fit: six, four, three, two, one.
const PER_VIEW: Record<number, number> = { 1280: 6, 1000: 4, 800: 3, 600: 2, 390: 1 };

for (const width of [1280, 1000, 800, 600, 390]) {
  test(`at ${width}, the visible job cards span the lanes card's width`, async ({ page, isMobile }) => {
    test.skip(isMobile, "each width is set here; the desktop project runs them all");
    await page.setViewportSize({ width, height: 900 });
    await page.goto("/ship");
    const row = page.getByRole("list", { name: "Commits", exact: true }).locator(":scope > li").first();
    await row.getByRole("button", { name: /^Open / }).click();
    await expect(row.getByRole("list", { name: "Stages" })).toBeVisible();
    const m = await row.evaluate((li) => {
      const track = li.querySelector(".ship-jobs__track");
      const lanes = li.querySelector('section[aria-label="Lanes"]');
      if (!track || !lanes) return null;
      const t = track.getBoundingClientRect();
      const shown = [...track.children].map((c) => c.getBoundingClientRect()).filter((r) => r.left >= t.left - 1 && r.right <= t.right + 1);
      const first = shown[0];
      const last = shown[shown.length - 1];
      const cs = getComputedStyle(lanes);
      const inner = lanes.getBoundingClientRect().width - parseFloat(cs.paddingLeft) - parseFloat(cs.paddingRight) - parseFloat(cs.borderLeftWidth) - parseFloat(cs.borderRightWidth);
      return { n: shown.length, span: first && last ? last.right - first.left : 0, lanes: inner };
    });
    expect(m).not.toBeNull();
    expect(m?.n).toBe(PER_VIEW[width]);
    expect(Math.abs((m?.span ?? 0) - (m?.lanes ?? 0))).toBeLessThanOrEqual(1);
    await expect(row.getByRole("button", { name: "Previous stages" })).toBeDisabled();
  });
}
