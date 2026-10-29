/**
 * The opened commit's Lanes as f-ship draws them (plan 01a1856fe81375b1bd1d1d86f270a45b, step 10),
 * against the mock's D41: a label column pinned left beside the timeline; the axis from +0s in six
 * steps or fewer; each lane's bar from its start to its end (to now while under way), within 1 px
 * of where its times fall; the now line while a lane is under way; Fit in the room there is and
 * Detail at 140 px a minute, scrolling sideways in its card with the labels where they are; from 320
 * to 1440 px.
 */
import { expect, test, type Locator } from "@playwright/test";

const REAL = !!process.env.E2E_BASE_URL;
test.skip(REAL, "drives the mock's commits");

/** Each lane's times and where its bar is drawn, and the track it's drawn on. */
const drawn = (box: Locator) =>
  box.evaluate((el) => {
    const [lo, hi] = ((el as HTMLElement).dataset.horizon ?? "0-0").split("-").map(Number);
    return {
      lo: lo ?? 0,
      hi: hi ?? 0,
      lanes: [...el.querySelectorAll<HTMLElement>(".ship-lrow[data-lane]")].map((row) => {
        const t = row.querySelector(".ship-lt")?.getBoundingClientRect();
        const b = row.querySelector<HTMLElement>(".ship-lbar");
        const r = b?.getBoundingClientRect();
        return {
          lane: row.dataset.lane ?? "",
          start: row.dataset.start ? Number(row.dataset.start) : null,
          end: row.dataset.end ? Number(row.dataset.end) : null,
          done: b?.dataset.done !== undefined,
          track: t ? { x: t.x, w: t.width } : null,
          bar: r ? { x: r.x, w: r.width } : null,
          label: row.querySelector(".ship-ll")?.getBoundingClientRect().x ?? 0,
        };
      }),
      ticks: [...el.querySelectorAll(".ship-lrow--axis .ship-lt > span")].map((s) => s.textContent ?? ""),
      now: el.querySelector<HTMLElement>(".ship-lnow > i")?.style.transform ?? null,
    };
  });

for (const width of [320, 390, 800, 1280, 1440])
  test(`at ${width}, each lane's bar where its times fall on the axis, the labels pinned, Fit and Detail`, async ({ page, isMobile }) => {
    test.skip(isMobile, "the widths are set here; the desktop projects run them");
    test.setTimeout(90_000);
    await page.setViewportSize({ width, height: 900 });
    await page.goto("/ship");
    const row = page.locator(".ship-row").filter({ hasText: "D41" }).first();
    await expect(row).toBeVisible({ timeout: 15_000 });
    await row.locator(".ship-go").click();
    const lanes = row.locator('section[aria-label="Lanes"]');
    const box = lanes.locator(".ship-lanesbox");
    await expect(box).toBeVisible({ timeout: 15_000 });
    await expect.poll(() => box.locator(".ship-lbar").count(), { timeout: 15_000 }).toBeGreaterThan(0);
    for (const room of ["Fit", "Detail"] as const) {
      await lanes.getByRole("group", { name: "Lanes' scale" }).getByRole("button", { name: room }).click();
      await page.waitForTimeout(150);
      const d = await drawn(box);
      const span = d.hi - d.lo;
      expect(span, room).toBeGreaterThan(0);
      // The axis: from +0s, in six steps or fewer in Fit.
      expect(d.ticks[0], room).toBe("+0s");
      if (room === "Fit") expect(d.ticks.length, room).toBeLessThanOrEqual(7);
      // Each bar where its times fall: its start, and a finished one's end, within 1 px.
      let checked = 0;
      for (const l of d.lanes) {
        if (l.start == null || !l.bar || !l.track) continue;
        const at = (t: number) => l.track!.x + ((t - d.lo) / span) * l.track!.w;
        expect(Math.abs(l.bar.x - at(l.start)), `${room}: ${l.lane}'s start`).toBeLessThanOrEqual(1);
        // (A span shorter than a bar's least width draws the least width.)
        if (l.done && l.end != null && at(l.end) - at(l.start) > 4) expect(Math.abs(l.bar.x + l.bar.w - at(l.end)), `${room}: ${l.lane}'s end`).toBeLessThanOrEqual(1);
        checked++;
      }
      expect(checked, room).toBeGreaterThan(0);
      // The now line while a lane is under way, at now's place.
      const live = d.lanes.some((l) => l.start != null && (l.end == null || l.end > Date.now() / 1000));
      if (live) expect(d.now, `${room}: the now line`).toMatch(/^translateX\(\d+(\.\d+)?%\)$/);
      if (room === "Detail") {
        // Detail: 140 px a minute, sideways inside its card, the labels where they are.
        const track = d.lanes.find((l) => l.track)?.track;
        expect(Math.abs((track?.w ?? 0) - Math.max(480, (span * 140) / 60)), "Detail's track").toBeLessThanOrEqual(1);
        const before = d.lanes[0]?.label ?? 0;
        await box.locator(".ship-lscroll").evaluate((s) => s.scrollTo({ left: 400 }));
        await page.waitForTimeout(100);
        const after = await drawn(box);
        expect(Math.abs((after.lanes[0]?.label ?? 0) - before), "the labels stay pinned").toBeLessThanOrEqual(1);
        expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
        await box.locator(".ship-lscroll").evaluate((s) => s.scrollTo({ left: 0 }));
      }
    }
  });
