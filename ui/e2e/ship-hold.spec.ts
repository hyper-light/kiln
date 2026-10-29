/**
 * Ship holding still (plan 01a175b7f20b7277b15bddd2ad001bcc, step 6), against the mock's fixed rows:
 * with the pointer on the list, a commit that arrives waits and no row moves; released, it shows.
 */
import { expect, test, type Page } from "@playwright/test";

const REAL = !!process.env.E2E_BASE_URL;
test.skip(REAL, "drives the mock's commits");

/** Each row's key and where it is on screen. */
const tops = (page: Page) => page.locator(".ship-list > [data-glide]").evaluateAll((els) => els.map((e) => `${(e as HTMLElement).dataset.glide}@${Math.round(e.getBoundingClientRect().top)}`));

test("rows hold while the pointer is on the list, and the new commit shows once it's released", async ({ page, isMobile }) => {
  test.skip(isMobile, "a pointer resting on the list is a desktop's gesture; a phone holds on touch and scroll");
  test.setTimeout(90_000);
  await page.request.post("/api/mock/ship/arrive?clear=1");
  try {
    await page.goto("/ship");
    const list = page.getByRole("list", { name: "Commits", exact: true });
    await expect(list.locator(":scope > li").first()).toBeVisible();
    const box = await list.boundingBox();
    if (!box) throw new Error("no list");
    const x = box.x + 40;
    const y = box.y + 20;
    await page.mouse.move(x, y);
    const before = await tops(page);
    await page.request.post("/api/mock/ship/arrive");
    const refetched = page.waitForResponse((r) => r.url().includes("/api/ship/commits"), { timeout: 40_000 });
    let fetched = false;
    void refetched.then(() => (fetched = true));
    for (let i = 0; !fetched && i < 100; i++) {
      await page.mouse.move(x + (i % 2) * 4, y + (i % 3));
      await page.waitForTimeout(350);
    }
    await refetched;
    await page.mouse.move(x + 2, y + 1);
    await page.waitForTimeout(300);
    expect(await tops(page)).toEqual(before);
    await page.mouse.move(box.x + box.width / 2, Math.max(1, box.y - 150));
    await expect(list.locator(":scope > li").first()).toContainText("arrived 1", { timeout: 15_000 });
  } finally {
    await page.request.post("/api/mock/ship/arrive?clear=1");
  }
});
