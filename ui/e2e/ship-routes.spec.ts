/**
 * Ship's route and nav (plan 01a175b7f20b7277b15bddd2ad001bcc, step 1), in the production build
 * against the mock: Ship sits in the nav where Runs and Deployments were; /runs and /deployments
 * are its views and keep their addresses and filters; a run's and an app's pages keep theirs, with
 * Ship the current section. Every old address lands on what it showed.
 */
import { expect, test, type Page } from "@playwright/test";

const REAL = !!process.env.E2E_BASE_URL;
test.skip(REAL, "drives the mock");

/** Ship is the current section in the main nav, and Runs and Deployments are no longer in it. */
async function shipCurrent(page: Page) {
  // The main nav shows from xl up; a phone has it behind the menu.
  if ((page.viewportSize()?.width ?? 0) < 1280) return;
  const nav = page.getByRole("navigation", { name: "Main navigation" });
  await expect(nav.getByRole("link", { name: "Ship", exact: true })).toHaveAttribute("aria-current", "page");
  await expect(nav.getByRole("link", { name: "Runs", exact: true })).toHaveCount(0);
  await expect(nav.getByRole("link", { name: "Deployments", exact: true })).toHaveCount(0);
}

/** The Ship view shown is `name`. */
async function view(page: Page, name: string) {
  await expect(page.getByRole("navigation", { name: "Ship views" }).getByRole("link", { name, exact: true })).toHaveClass(/is-current/);
}

/** The first link on the page whose address starts with `prefix` and goes further. */
async function firstBelow(page: Page, prefix: string) {
  const link = page
    .locator(`a[href^="${prefix}"]`)
    .filter({ hasNot: page.locator(`[href="${prefix}"]`) })
    .first();
  await expect(link).toBeAttached();
  const href = await link.getAttribute("href");
  expect(href && href.length > prefix.length).toBeTruthy();
  return href ?? prefix;
}

test("/ship is Ship's own address, current in the nav", async ({ page }) => {
  await page.goto("/ship");
  await expect(page.getByRole("heading", { name: "Ship.", exact: true })).toBeVisible();
  await view(page, "Commits");
  await shipCurrent(page);
});

test("/runs lands on the runs, Ship's Runs view, its filters kept", async ({ page }) => {
  await page.goto("/runs?status=failed");
  await expect(page.getByRole("heading", { name: "Runs", exact: true })).toBeVisible();
  await expect(page).toHaveURL(/\/runs\?status=failed$/);
  await view(page, "Runs");
  await shipCurrent(page);
});

test("/runs/:id lands on the run, Ship current", async ({ page }) => {
  await page.goto("/runs");
  const href = await firstBelow(page, "/runs/");
  await page.goto(href);
  await expect(page).toHaveURL(new RegExp(`${href.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}$`));
  await expect(
    // The crumb reads "02 / Runs": Ship's number, then the runs.
    page.getByRole("navigation", { name: "Breadcrumb" }).getByRole("link", { name: /^02 \/ Runs$/ }),
  ).toBeVisible();
  await shipCurrent(page);
});

test("/deployments lands on the apps, Ship's Deployments view", async ({ page }) => {
  await page.goto("/deployments");
  await expect(page.getByRole("heading", { name: "Deployments.", exact: true })).toBeVisible();
  await view(page, "Deployments");
  await shipCurrent(page);
});

test("an app's page keeps its address, Ship current", async ({ page }) => {
  await page.goto("/deployments");
  const href = await firstBelow(page, "/deployments/apps/");
  await page.goto(href);
  await expect(page).toHaveURL(new RegExp(`${href.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}$`));
  await expect(page.getByRole("navigation", { name: "Breadcrumb" })).toBeVisible();
  await shipCurrent(page);
});
