/**
 * A commit's own page (Commit page plan 01a175b7f26e724d9159b4120a514fee, step 1), against the mock's
 * fixed rows: an opened row's Commit link leads to /ship/{owner}/{name}/{sha}, which loads; every
 * commit Ship lists has its page; one kiln doesn't know says so instead of breaking.
 */
import { expect, test } from "@playwright/test";

const REAL = !!process.env.E2E_BASE_URL;
test.skip(REAL, "drives the mock's commits");

test("Commit on an opened row leads to the commit's own page", async ({ page }) => {
  await page.goto("/ship");
  const row = page.locator(".ship-row").filter({ hasText: "D41" }).first();
  await expect(row).toBeVisible({ timeout: 15_000 });
  await row.locator(".ship-go").click();
  const link = row.getByRole("link", { name: /^Commit [0-9a-f]{7}$/ });
  await expect(link).toBeVisible({ timeout: 15_000 });
  const short = ((await link.textContent()) ?? "").replace("Commit ", "");
  await link.click();
  await expect(page).toHaveURL(new RegExp(`/ship/hyper-light/kiln/${short}[0-9a-f]*$`));
  await expect(page.getByRole("heading", { level: 1 })).toHaveText("pipeline: dynamic matrices");
  await expect(page.getByRole("region", { name: "Commit" })).toContainText(short);
});

test("every commit Ship lists has its page, and one kiln doesn't know says so", async ({ page, isMobile }) => {
  test.skip(isMobile, "the desktop projects visit them all");
  test.setTimeout(120_000);
  const list = (await (await page.request.get("/api/ship/commits")).json()) as { commits: { repo: string; sha: string; short: string; message: string }[] };
  expect(list.commits.length).toBeGreaterThan(0);
  for (const c of list.commits) {
    await page.goto(`/ship/${c.repo}/${c.sha}`);
    const commit = page.getByRole("region", { name: "Commit" });
    await expect(commit, `${c.short}'s page`).toContainText(c.short, { timeout: 15_000 });
    if (c.message) await expect(page.getByRole("heading", { level: 1 })).toHaveText(c.message);
  }
  await page.goto("/ship/hyper-light/kiln/0000000000000000000000000000000000000000");
  await expect(page.getByRole("alert")).toContainText("Couldn't load the commit");
});
