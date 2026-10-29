/**
 * The opened commit's Lanes keep their scale (plan 01a175b7f21b7455bcdcd71e94ecdce0, step 2), against
 * the mock's fixed rows: over 60 s of live updates, every bar keeps its start and every finished bar
 * its span. And the horizon holds while the row is open (Agent pace and checks plan
 * 01a175b7f2837279be2a229ee94ac1f0, step 2): past one of its 10-minute steps nothing moves, and Now
 * catches it up.
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

test("over 60 s the lanes' bars keep their scale", async ({ page, isMobile }) => {
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
  // One horizon all the while (it holds while the row is open), every bar where it was.
  expect(new Set(samples.map((s) => s.span)).size).toBe(1);
  for (let i = 1; i < samples.length; i++) {
    const [was, now] = [samples[i - 1], samples[i]];
    if (!was || !now) continue;
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

test("open, the horizon holds past a 10-minute step, and Now catches it up", async ({ page, isMobile }) => {
  test.skip(isMobile, "the desktop projects run it");
  test.setTimeout(90_000);
  // The page's clock, so ten minutes can pass at once (the mock's times stay real).
  await page.clock.install();
  await page.setViewportSize({ width: 1280, height: 900 });
  await page.goto("/ship");
  const row = page.locator(".ship-row").filter({ hasText: "D41" }).first();
  await expect(row).toBeVisible({ timeout: 15_000 });
  await row.locator(".ship-go").click();
  const lanes = row.locator('section[aria-label="Lanes"]');
  await expect(lanes.locator(".ship-lanesbox")).toBeVisible({ timeout: 15_000 });
  await expect.poll(() => lanes.locator(".ship-lanesbox .ship-lane__bar").count(), { timeout: 15_000 }).toBeGreaterThan(0);
  const now = lanes.getByRole("button", { name: "Now" });
  await expect(now).toBeDisabled();
  const before = await sample(page);
  // Ten minutes on: the page polls, draws again, and the horizon it would draw now is a step on.
  await page.clock.fastForward("10:00");
  await expect(now).toBeEnabled({ timeout: 30_000 });
  const after = await sample(page);
  expect(after.span).toBe(before.span);
  const was = new Map(before.bars.map((b) => [b.name, b]));
  for (const b of after.bars) {
    const then = was.get(b.name);
    if (!then) continue;
    expect(b.left, `${b.name}'s start`).toBe(then.left);
    if (b.width !== null && then.width !== null) expect(b.width, `${b.name}'s span`).toBe(then.width);
  }
  await now.click();
  await expect(now).toBeDisabled();
  expect((await sample(page)).span).not.toBe(before.span);
});
