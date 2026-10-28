/**
 * The opened commit's Lanes keep their scale (plan 01a175b7f21b7455bcdcd71e94ecdce0, step 2), against
 * the mock's fixed rows: over 60 s of live updates, every bar keeps its start and every finished bar
 * its span, between the horizon's 10-minute steps (at most one of them in a minute).
 */
import { expect, test, type Page } from "@playwright/test";

const REAL = !!process.env.E2E_BASE_URL;
test.skip(REAL, "drives the mock's commits");

/** The window's span and each bar's place, by its lane's name. */
const sample = (page: Page) =>
  page
    .locator(".ship-row")
    .filter({ hasText: "D41" })
    .first()
    .locator(".ship-lanesbox")
    .evaluate((box) => ({
      span: box.querySelector(".ship-lanesbox__span")?.textContent ?? "",
      bars: [...box.querySelectorAll(".ship-lane")].flatMap((lane) => {
        const bar = lane.querySelector<HTMLElement>(".ship-lane__bar");
        if (!bar) return [];
        const name = lane.querySelector(".ship-lane__name")?.textContent ?? "";
        // A finished span says when it ended; a bar under way grows to now, so only its start is kept.
        const done = (lane.querySelector(".ship-lane__when")?.textContent ?? "").includes("–") || bar.classList.contains("ship-lane__bar--point");
        return [{ name, left: bar.style.left, width: done ? bar.style.width : null }];
      }),
    }));

test("over 60 s the lanes' bars keep their scale, but for a 10-minute step", async ({ page, isMobile }) => {
  test.skip(isMobile, "the desktop projects run it");
  test.setTimeout(150_000);
  await page.setViewportSize({ width: 1280, height: 900 });
  await page.goto("/ship");
  const row = page.locator(".ship-row").filter({ hasText: "D41" }).first();
  await expect(row).toBeVisible({ timeout: 15_000 });
  await row.locator(".ship-go").click();
  // The lanes drawn, with their bars (a point has no box of its own, so it's counted, not seen).
  await expect(row.locator(".ship-lanesbox")).toBeVisible({ timeout: 15_000 });
  await expect.poll(() => row.locator(".ship-lanesbox .ship-lane__bar").count(), { timeout: 15_000 }).toBeGreaterThan(0);
  const samples = [await sample(page)];
  for (let i = 0; i < 6; i++) {
    await page.waitForTimeout(10_000);
    samples.push(await sample(page));
  }
  expect(samples[0]?.bars.length ?? 0).toBeGreaterThan(0);
  // At most one 10-minute step in a minute; between steps, every bar where it was.
  expect(new Set(samples.map((s) => s.span)).size).toBeLessThanOrEqual(2);
  for (let i = 1; i < samples.length; i++) {
    const [was, now] = [samples[i - 1], samples[i]];
    if (!was || !now || was.span !== now.span) continue;
    const before = new Map(was.bars.map((b) => [b.name, b]));
    for (const b of now.bars) {
      const then = before.get(b.name);
      // A lane by another name is another lane (the mock's fixed rows stream whichever job runs).
      if (!then) continue;
      expect(b.left, `${b.name}'s start`).toBe(then.left);
      if (b.width !== null && then.width !== null) expect(b.width, `${b.name}'s span`).toBe(then.width);
    }
  }
});
