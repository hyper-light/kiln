/**
 * Ship's route and nav (plan 01a175b7f20b7277b15bddd2ad001bcc, step 1; its views as Ada approved
 * them on 2026-10-30, docs/ci.md W26-R24), in the production build against the mock: Ship sits in
 * the nav where Runs and Deployments were; its views are Commits and Deploys, /runs landing on its
 * commits and /deployments on its deploys; a run's and an app's pages keep their addresses, with
 * Ship the current section.
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

test("/ship is Ship's own address, current in the nav", async ({ page }) => {
  await page.goto("/ship");
  await expect(page.getByRole("heading", { name: "Ship.", exact: true })).toBeVisible();
  await view(page, "Commits");
  await shipCurrent(page);
});

test("/runs lands on Ship's commits", async ({ page }) => {
  await page.goto("/runs?status=failed");
  await expect(page).toHaveURL(/\/ship$/);
  await expect(page.getByRole("heading", { name: "Ship.", exact: true })).toBeVisible();
  await view(page, "Commits");
  await shipCurrent(page);
});

test("/runs/:id lands on the run, Ship current", async ({ page }) => {
  const runs = (await (await page.request.get("/api/runs?limit=1")).json()) as { id: string }[];
  const href = `/runs/${runs[0]?.id ?? ""}`;
  await page.goto(href);
  await expect(page).toHaveURL(new RegExp(`${href.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}$`));
  await expect(
    // The crumb reads "02 / Runs": Ship's number, then the runs.
    page.getByRole("navigation", { name: "Breadcrumb" }).getByRole("link", { name: /^02 \/ Runs$/ }),
  ).toBeVisible();
  await shipCurrent(page);
});

test("/deployments lands on Ship's deploys", async ({ page }) => {
  await page.goto("/deployments");
  await expect(page).toHaveURL(/\/ship\/deploys$/);
  await expect(page.getByRole("heading", { name: "Ship.", exact: true })).toBeVisible();
  await view(page, "Deploys");
  await shipCurrent(page);
});

test("an app's page keeps its address, Ship current", async ({ page }) => {
  const list = (await (await page.request.get("/api/cd/apps?limit=1")).json()) as { apps: { app: string }[] };
  const href = `/deployments/apps/${list.apps[0]?.app ?? ""}`;
  await page.goto(href);
  await expect(page).toHaveURL(new RegExp(`${href.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}$`));
  await expect(page.getByRole("navigation", { name: "Breadcrumb" })).toBeVisible();
  await shipCurrent(page);
});
