/**
 * P3.2 (kiln plan 01a1dd2e…; docs/isolation.md, "Warm pools"): a runner that starts microVMs shows the warm pools kiln
 * names on it in its details, in the production build against the mock's Mac mini: each pool's agent (its definition's
 * name, a deployment experiment's variant after it, and nothing of another person's agent), its size, its hits and
 * when it's let go. A runner that runs none (the mock's metal-ryzen-02) has no such list. Nothing scrolls sideways. On a
 * desktop and a phone.
 */
import { expect, test, type Page } from "@playwright/test";
import { MAC_MINI } from "../mock/data.ts";

/** Against a real kiln server (playwright.config.ts): these read the mock's pools. */
const REAL = !!process.env.E2E_BASE_URL;

test.skip(REAL, "reads the mock's pools");

/** The name the mock's Mac mini starts with. */
const LABEL = "metal-mac-mini";

const rows = (page: Page) => page.locator("tr[data-href]");
const details = (page: Page, name: string) => page.getByRole("region", { name: `${name} in depth` });

/** Nothing in the runner's details wider than the page. */
async function nothingSideways(page: Page) {
  const seen = await page.evaluate(() => {
    const client = document.documentElement.clientWidth;
    return {
      page: document.documentElement.scrollWidth - client,
      over: [...document.querySelectorAll(".runner-details, .runner-details *")]
        .filter((e) => e.getBoundingClientRect().right > client + 0.5)
        .map((e) => `${e.tagName.toLowerCase()}.${String(e.className)}`),
    };
  });
  expect(seen.page, "the page scrolls sideways").toBeLessThanOrEqual(0);
  expect(seen.over, "something in the runner's details is wider than the page").toEqual([]);
}

test("a runner's details show each warm pool's agent, size and hits", async ({ page }, info) => {
  expect((await page.request.post(`/api/agents/${MAC_MINI}/label`, { data: { label: LABEL } })).ok()).toBe(true);
  await page.goto("/runners");
  await rows(page).filter({ hasText: LABEL }).click();
  await expect(page).toHaveURL(new RegExp(`[?&]agent=${MAC_MINI}(&|$)`));

  const pools = details(page, LABEL).getByRole("list", { name: `Warm pools on ${LABEL}` });
  const pool = pools.getByRole("listitem");
  await expect(pool).toHaveCount(3);
  await expect(pool.nth(0)).toContainText("reviewer");
  await expect(pool.nth(0)).toContainText("2 vCPUs · 2.0 GiB");
  await expect(pool.nth(0)).toContainText("4 hits · 5 starts");
  await expect(pool.nth(0)).toContainText(/let go in \d+ min/);
  await expect(pool.nth(1)).toContainText("reviewer · b of faster-start");
  await expect(pool.nth(1)).toContainText("4 vCPUs · 4.0 GiB");
  await expect(pool.nth(1)).toContainText("1 hit · 2 starts");
  await expect(pool.nth(2)).toContainText("Another person's agent");
  await expect(pool.nth(2)).toContainText("1 vCPU · 512 MiB");
  await expect(pool.nth(2)).toContainText("0 hits · 1 start");
  await nothingSideways(page);
  await page.screenshot({ path: info.outputPath(`warm-pools-${info.project.name}.png`), fullPage: true });
});

test("a runner that runs no microVMs has no warm pools", async ({ page }) => {
  // Its facts say it runs none (ui/mock/data.ts MICROVM_RUNNERS), so its details have no warm pools.
  const name = "metal-ryzen-02";
  await page.goto("/runners");
  await rows(page).filter({ hasText: name }).click();
  await expect(page).toHaveURL(new RegExp(`[?&]agent=${name}(&|$)`));
  await expect(details(page, name)).toBeVisible();
  await expect(page.getByRole("list", { name: /^Warm pools on / }), `no warm pools on ${name}`).toHaveCount(0);
});
