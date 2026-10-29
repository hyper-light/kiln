/**
 * An opened commit stays put as updates arrive (plan 01a175b7f21b7455bcdcd71e94ecdce0, step 3),
 * against the mock's fixed rows: page the job cards, or walk the lanes in Detail, focus a control in
 * the open row, let the list refresh while a commit arrives, and the cards keep their view, the lanes
 * their walk, room and rows, the focus and the page stay, and the row stays open in its place.
 */
import { expect, test } from "@playwright/test";

const REAL = !!process.env.E2E_BASE_URL;
test.skip(REAL, "drives the mock's commits");

test("the job cards' view and focus in an open row survive updates", async ({ page, isMobile }) => {
  test.skip(isMobile, "the width is set here; the desktop projects run it");
  test.setTimeout(90_000);
  await page.request.post("/api/mock/ship/arrive?clear=1");
  try {
    // Three cards to a view: the six stages page once.
    await page.setViewportSize({ width: 800, height: 900 });
    await page.goto("/ship");
    const row = page.locator(".ship-row").filter({ hasText: "D41" }).first();
    await expect(row).toBeVisible({ timeout: 15_000 });
    await row.locator(".ship-go").click();
    // The second view: staging, production, replant.
    const first = row.locator(".ship-jobs__track > .ship-job").first();
    await expect(first).toHaveAttribute("data-stage", "land");
    await row.getByRole("button", { name: "Next stages" }).click();
    await expect(first).toHaveAttribute("data-stage", "staging");
    const copy = row.getByRole("button", { name: /^Cop(y|ied) the output$/ });
    await copy.focus();
    // Updates: a commit arrives, and the list fetches again (twice).
    await page.request.post("/api/mock/ship/arrive");
    for (let i = 0; i < 2; i++) await page.waitForResponse((r) => r.url().includes("/api/ship/commits") && r.request().method() === "GET", { timeout: 40_000 });
    await page.waitForTimeout(300);
    await expect(first).toHaveAttribute("data-stage", "staging");
    await expect(row.locator(".ship-jobs__track > .ship-job")).toHaveCount(3);
    await expect(copy).toBeFocused();
    await expect(row.locator(".ship-go")).toHaveAttribute("aria-expanded", "true");
  } finally {
    await page.request.post("/api/mock/ship/arrive?clear=1");
  }
});

test("the lanes' walk, room, rows and focus in an open row survive updates", async ({ page, isMobile }) => {
  test.skip(isMobile, "the width is set here; the desktop projects run it");
  test.setTimeout(90_000);
  await page.request.post("/api/mock/ship/arrive?clear=1");
  try {
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/ship");
    const row = page.locator(".ship-row").filter({ hasText: "D41" }).first();
    await expect(row).toBeVisible({ timeout: 15_000 });
    await row.locator(".ship-go").click();
    const lanes = row.locator('section[aria-label="Lanes"]');
    const box = lanes.locator(".ship-lanesbox");
    await expect(box).toBeVisible({ timeout: 15_000 });
    // Detail, and a step back along the horizon.
    await lanes.getByRole("button", { name: "Detail" }).click();
    await lanes.getByRole("button", { name: "Earlier" }).click();
    await expect(lanes.getByRole("button", { name: "Now" })).toBeEnabled();
    // Each lane's row marked with its name, to tell a row kept from one drawn again. (A lane can be
    // another's: the mock's fixed rows stream whichever of its jobs runs. Those that stay are kept.)
    const kept = await box.locator(".ship-lane").evaluateAll((els) => els.map((el) => ((el as HTMLElement).dataset.probe = el.querySelector(".ship-lane__name")?.textContent ?? "")));
    expect(kept.length).toBeGreaterThan(0);
    const later = lanes.getByRole("button", { name: "Later" });
    await later.focus();
    const y = await page.evaluate(() => window.scrollY);
    // Updates: a commit arrives, and the list fetches again (twice).
    await page.request.post("/api/mock/ship/arrive");
    for (let i = 0; i < 2; i++) await page.waitForResponse((r) => r.url().includes("/api/ship/commits") && r.request().method() === "GET", { timeout: 40_000 });
    await page.waitForTimeout(300);
    await expect(lanes.getByRole("button", { name: "Fit" })).toBeVisible();
    await expect(lanes.getByRole("button", { name: "Now" })).toBeEnabled();
    const after = await box
      .locator(".ship-lane")
      .evaluateAll((els) => els.map((el) => ({ name: el.querySelector(".ship-lane__name")?.textContent ?? "", probe: (el as HTMLElement).dataset.probe ?? null })));
    const stayed = after.filter((l) => kept.includes(l.name));
    expect(stayed.length).toBeGreaterThan(0);
    expect(stayed.filter((l) => l.probe !== l.name)).toEqual([]);
    await expect(later).toBeFocused();
    expect(await page.evaluate(() => window.scrollY)).toBe(y);
    await expect(row.locator(".ship-go")).toHaveAttribute("aria-expanded", "true");
  } finally {
    await page.request.post("/api/mock/ship/arrive?clear=1");
  }
});
