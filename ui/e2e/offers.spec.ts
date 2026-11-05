/**
 * Runners → Offers (docs/reach.md §2.1; AUDIT O1, O2), in the production build against the mock's
 * fleet: each runner with what it can take (its answer for each ask size, when it would start),
 * why one isn't taking work, one before offers and one offline; what waits, by need and size, a
 * page as long as the fleet, the next from its last group. The page reads again only when the
 * server's `offers` event names a newer version than it shows, and then with its ETag. Nothing
 * scrolls sideways, the longest runner name included. Desktop and phone; Chromium, and WebKit with
 * E2E_WEBKIT=1.
 */
import { expect, test, type Page } from "@playwright/test";

test.skip(!!process.env.E2E_BASE_URL, "reads the mock's fleet and its test hooks");

const runners = (page: Page) => page.getByRole("list", { name: "Runners" }).locator(":scope > li");
const groups = (page: Page) => page.getByRole("list", { name: "Waiting work" }).locator(":scope > li");
const reads = async (page: Page) => (await (await page.request.get("/api/mock/offers/reads")).json()) as { full: number; unchanged: number };

async function nothingSideways(page: Page) {
  const seen = await page.evaluate(() => {
    const client = document.documentElement.clientWidth;
    return {
      page: document.documentElement.scrollWidth - client,
      over: [...document.querySelectorAll(".offers *")].filter((e) => e.getBoundingClientRect().right > client + 0.5).map((e) => `${e.tagName.toLowerCase()}.${String(e.className)}`),
    };
  });
  expect(seen.page, "the page scrolls sideways").toBeLessThanOrEqual(0);
  expect(seen.over, "something on the page is wider than it").toEqual([]);
}

test("each runner says what it can take, and what waits says why", async ({ page }) => {
  await page.goto("/runners");
  await page.getByRole("link", { name: "Offers", exact: true }).click();
  await expect(page).toHaveURL(/\/runners\/offers$/);
  await expect(page.getByRole("heading", { level: 1, name: "Offers" })).toBeVisible();

  await expect(runners(page)).toHaveCount(4);
  const linux = runners(page).filter({ hasText: "adacachy" });
  await expect(linux).toContainText("takes work");
  await expect(linux).toContainText("3 wait: 1 interactive, 2 batch");
  await expect(linux.getByRole("list", { name: "What adacachy would do" }).locator("li")).toHaveText([
    /4 CPUs · 8 GiB\s*starts now/,
    /16 CPUs · 32 GiB\s*waits: runs ahead of it go first, starts in about 3 minutes \(at the latest in about 41 minutes\)/,
    /8 CPUs · 32 GiB · 1 GPU\s*never: past its budget of GPUs \(from its room\)/,
  ]);
  await expect(runners(page).filter({ hasText: "adas-macbook-pro" })).toContainText("memory pressure 41%");
  await expect(runners(page).filter({ hasText: "metal-pi-04" })).toContainText("says nothing of its admission");
  await expect(runners(page).filter({ hasText: "fedora.attlocal.net" })).toContainText("offline");
  await expect(page.getByText("2 of 4 taking work")).toBeVisible();

  // A page of what waits as long as the fleet (4 of 6 groups), then the rest.
  await expect(groups(page)).toHaveCount(4);
  await expect(groups(page).filter({ hasText: "linux/amd64 +gpu" })).toContainText("2 wait: no runner has what it needs");
  await expect(groups(page).first()).toContainText("any runner");
  await nothingSideways(page);
  await page.getByRole("link", { name: "Next 4" }).click();
  await expect(page).toHaveURL(/after=/);
  await expect(groups(page)).toHaveCount(2);
  await expect(groups(page).filter({ hasText: "windows" })).toContainText("no runner that could is connected");
  await nothingSideways(page);
  await page.getByRole("link", { name: "First page" }).click();
  await expect(groups(page)).toHaveCount(4);
});

test("the page reads again only for a newer version, and then by its ETag", async ({ page }) => {
  await page.goto("/runners/offers");
  await expect(runners(page)).toHaveCount(4);
  const aside = page.getByText(/^\d+ runs in 6 groups$/);
  const before = Number((await aside.innerText()).split(" ")[0]);
  // Let the events socket open, and the page's first read settle.
  await page.waitForFunction(() => document.readyState === "complete");
  await page.waitForTimeout(1500);
  const first = await reads(page);

  // The version it has: nothing is read.
  await page.request.post("/api/mock/offers/same");
  await page.waitForTimeout(2000);
  expect(await reads(page)).toEqual(first);

  // A newer one: read once, and what changed is shown.
  await page.request.post("/api/mock/offers/change");
  await expect(page.getByText(`${before + 1} runs in 6 groups`)).toBeVisible();
  const after = await reads(page);
  expect(after.full + after.unchanged - first.full - first.unchanged).toBe(1);
});
