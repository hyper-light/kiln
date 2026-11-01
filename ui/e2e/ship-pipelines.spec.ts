/**
 * Ship's Pipelines (docs/ci.md W29-R01 to R08, as Ada approved its stills on 2026-10-31), against
 * the mock's three repositories, at the stills' widths: 1280 on a desktop project, 390 on a phone's,
 * in Chromium and (E2E_WEBKIT=1) WebKit. Pipelines is Ship's third tab; its rows are Ship's by
 * repository, needs-someone first, DRAFT beside a draft's title, and a New row last; `?repo=&path=`
 * opens a row and brings it into view; nothing scrolls sideways.
 */
import { expect, test, type Page } from "@playwright/test";

const REAL = !!process.env.E2E_BASE_URL;
test.skip(REAL, "drives the mock's pipelines");

/** The stills' widths: 1280 for a desktop, 390 for a phone. */
const atStillWidth = (page: Page, isMobile: boolean) => page.setViewportSize(isMobile ? { width: 390, height: 844 } : { width: 1280, height: 900 });

/** The app's own requests waiting on you come and go over the page's foot: set aside, as Deploys' spec does. */
const notices = (page: Page) => page.addStyleTag({ content: ".rq-viewport { display: none !important; }" });

const list = (page: Page) => page.getByRole("list", { name: "Workflows and actions by repository" });
/** A repository's rows, in the list's order, up to the next heading. */
const rowsOf = (page: Page, repo: string) => list(page).locator(`.ppl-row[data-key^="${repo}:"]:not(.ppl-new)`);

/** How far the page scrolls sideways. */
const sideways = (page: Page) => page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);

test("Pipelines is Ship's third tab: repositories needing someone first, each row Ship's, DRAFT beside a draft's title, New last", async ({ page, isMobile }) => {
  await atStillWidth(page, isMobile);
  await page.goto("/ship/pipelines");
  await notices(page);
  const views = page.getByRole("navigation", { name: "Ship views" });
  await expect(views.getByRole("link")).toHaveText(["Commits", "Deploys", "Pipelines"]);
  await expect(views.getByRole("link", { name: "Pipelines" })).toHaveClass(/is-current/);
  await expect(page.getByPlaceholder("Repository, workflow, action, kind, machine…")).toBeVisible();

  // Repositories with rows needing someone first, then by name; each heading says what's there.
  await expect(list(page).locator(".dp-proj > span:first-child")).toHaveText(["hyper-light/ci-kit", "hyper-light/kiln", "hyper-light/quiver"]);
  await expect(list(page).locator(".dp-proj > span:last-child")).toHaveText([
    "shared · 6 actions · 1 workflow · 1 draft",
    "7 workflows · 1 failing · 1 waits on you",
    "3 workflows · ci-kit v2.4.0 is out",
  ]);
  await expect(list(page).locator(".dp-proj > span[data-needs]")).toHaveCount(2);

  // kiln's: Review, then Failing, then the rest by name.
  const kiln = rowsOf(page, "hyper-light/kiln");
  await expect(kiln.locator(".ppl-title")).toHaveText(["stress", "e2e", "ci", "GitHub CI", "nightly", "release", "smoke"]);
  await expect(kiln.locator(".ship-stand")).toHaveText(["Review", "Failing", "Passing", "Not run here", "Passing", "Passing", "Passing"]);
  // DRAFT sits beside a draft's title, and nowhere else.
  await expect(kiln.locator(".ppl-draft")).toHaveCount(1);
  await expect(kiln.first().locator(".ppl-name")).toContainText("stressDRAFT");
  // Where each is: a draft's change, else its branch; an action's newest tag.
  await expect(kiln.first().locator(".ppl-ref[data-ref=change]")).toHaveText("#214");
  await expect(kiln.nth(2).locator(".ppl-ref")).toHaveText("main");
  await expect(rowsOf(page, "hyper-light/ci-kit").filter({ hasText: "setup-rust" }).locator(".ppl-ref[data-ref=tag]")).toHaveText("v2.4.0");
  // Three pips each: Checks, Runs on, Latest.
  await expect(kiln.nth(1).locator(".js-p")).toHaveCount(3);

  // Each repository ends with its New row.
  for (const repo of ["hyper-light/ci-kit", "hyper-light/kiln", "hyper-light/quiver"]) {
    const keys = await list(page)
      .locator(".ppl-row")
      .evaluateAll((els) => els.map((e) => e.getAttribute("data-key") ?? ""));
    const mine = keys.filter((k) => k.startsWith(`${repo}:`));
    expect(mine.at(-1)).toBe(`${repo}:new`);
  }
  expect(await sideways(page)).toBe(0);
});

test("?repo=&path= opens that row and brings it into view", async ({ page, isMobile }) => {
  await atStillWidth(page, isMobile);
  await page.goto("/ship/pipelines?repo=hyper-light%2Fkiln&path=.kiln%2Fci.yaml");
  await notices(page);
  const row = list(page).locator('.ppl-row[data-key="hyper-light/kiln:.kiln/ci.yaml"]');
  await expect(row.locator(".ship-main")).toHaveAttribute("aria-expanded", "true");
  await expect(row.locator(".ppl-kicker")).toContainText(".kiln/ci.yaml · kiln's dialect · main · a3f2c1d");
  await expect(row).toBeInViewport();
  // Closing it takes it out of the address; another opens in its place.
  await row.locator(".ship-go").click();
  await expect(page).not.toHaveURL(/path=/);
  expect(await sideways(page)).toBe(0);
});
