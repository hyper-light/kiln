/**
 * An opened commit's section heads as f-ship draws them (plan 01a1856fe81375b1bd1d1d86f270a45b, step
 * 7), against the mock's D41: each section's label on the left above its card and its controls on
 * the right (Files' count, Jobs' previous and next, Lanes' Fit and Detail with Earlier, Now and
 * Later), no label inside a card, and from 320 to 1440 px no control over another or over its label.
 */
import { expect, test } from "@playwright/test";

const REAL = !!process.env.E2E_BASE_URL;
test.skip(REAL, "drives the mock's commits");

interface Box {
  x: number;
  y: number;
  w: number;
  h: number;
}

const apart = (a: Box, b: Box) => a.x + a.w <= b.x + 0.5 || b.x + b.w <= a.x + 0.5 || a.y + a.h <= b.y + 0.5 || b.y + b.h <= a.y + 0.5;

test("each section's head: its label on the left over its card, its controls on the right, none over another, 320 to 1440 px", async ({ page, isMobile }) => {
  test.skip(isMobile, "the widths are set here; the desktop projects run them");
  test.setTimeout(120_000);
  await page.setViewportSize({ width: 1280, height: 900 });
  await page.goto("/ship");
  const row = page.locator(".ship-row").filter({ hasText: "D41" }).first();
  await expect(row).toBeVisible({ timeout: 15_000 });
  await row.locator(".ship-go").click();
  await expect(row.locator(".ship-sech > h3")).toHaveText(["Output", "Jobs", "Lanes"], { timeout: 15_000 });
  await expect(row.locator(".ship-lanesbox")).toBeVisible({ timeout: 15_000 });
  for (const width of [320, 390, 640, 800, 1080, 1280, 1440]) {
    await page.setViewportSize({ width, height: 900 });
    await page.waitForTimeout(150);
    const heads = await row.locator(".ship-sech").evaluateAll((els) =>
      els.map((h) => {
        const box = (e: Element) => {
          const r = e.getBoundingClientRect();
          return { x: r.x, y: r.y, w: r.width, h: r.height };
        };
        const label = h.querySelector("h3");
        const controls = [...h.querySelectorAll(".ship-sech__ctl > *")].map(box);
        const card = h.nextElementSibling;
        return {
          label: label?.textContent ?? "",
          labelBox: label ? box(label) : null,
          head: box(h),
          controls,
          card: card ? box(card) : null,
          cardLabel: card?.querySelector("h3")?.textContent ?? null,
        };
      }),
    );
    const at = (what: string) => `at ${width}: ${what}`;
    expect(heads.map((h) => h.label)).toEqual(["Output", "Jobs", "Lanes"]);
    for (const h of heads) {
      if (!h.labelBox || !h.card) throw new Error(at(`${h.label} has no label or no card`));
      // The label on the left, the controls to its right and inside the head, none over another.
      expect(Math.round(h.labelBox.x - h.head.x), at(`${h.label}'s label starts the head`)).toBe(0);
      for (const [i, c] of h.controls.entries()) {
        expect(apart(c, h.labelBox), at(`${h.label}'s control ${i} clear of its label`)).toBe(true);
        expect(c.x + c.w, at(`${h.label}'s control ${i} inside the head`)).toBeLessThanOrEqual(h.head.x + h.head.w + 0.5);
        for (const d of h.controls.slice(i + 1)) expect(apart(c, d), at(`${h.label}'s controls apart`)).toBe(true);
      }
      // Over its card, and no label inside the card.
      expect(h.card.y, at(`${h.label}'s card under its head`)).toBeGreaterThanOrEqual(h.head.y + h.head.h - 0.5);
      expect(h.cardLabel, at(`${h.label}'s card holds no label`)).toBeNull();
    }
    expect(heads.find((h) => h.label === "Jobs")?.controls.length, at("Jobs' previous and next")).toBe(2);
    expect(heads.find((h) => h.label === "Lanes")?.controls.length, at("Lanes' scale, Earlier, Now and Later")).toBe(4);
  }
  // Files' head (Review's) the same way: its label on the left, its count on the right.
  const files = row.locator('section[aria-label="Files"] > h3');
  await expect(files.locator(".rv-sum")).toHaveText(/^\d+ files · \+\d+ −\d+ · v\d+$/);
});
