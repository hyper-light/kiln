/**
 * Ship's rows are repositories (docs/ci.md W26-R29, W26-AC20; Ada, 2026-11-01): each row its
 * repository's newest commit; opened, its first section is its commits, a page at a time, and the
 * sections under the divider are the picked commit's. Against the mock: kiln's ten commits, and two
 * other repositories (hyper-raft's newest a failure that needs you, acme/web's in production).
 */
import { expect, test, type Locator } from "@playwright/test";
import { pickCommit, repoRow } from "./ship-pick";

const REAL = !!process.env.E2E_BASE_URL;
test.skip(REAL, "drives the mock's repositories");

const lines = (row: Locator) => row.locator("section.ship-commits .sc-c");
const pressed = (row: Locator, name: string) => row.locator("section.ship-commits").getByRole("button", { name, exact: true });

test("Ship's rows are repositories, needs-you first, each its newest commit's stand and line", async ({ page }) => {
  await page.goto("/ship");
  const rows = page.getByRole("list", { name: "Repositories", exact: true }).locator(":scope > li");
  await expect(rows.first()).toBeVisible({ timeout: 15_000 });
  // Needs you first: kiln's and hyper-raft's newest each need you, acme/web's is in production.
  await expect(rows.locator(":scope > .ship-r .ship-name")).toHaveText(["hyper-light/kiln", "hyper-light/hyper-raft", "acme/web"]);
  // kiln's row: its newest commit (D41's 4444444), its message before what it's doing.
  const kiln = repoRow(page);
  await expect(kiln.locator(".ship-text")).toHaveText(/^pipeline: dynamic matrices · /);
  await expect(kiln.locator(".ship-sha")).toHaveText("4444444");
  await expect(kiln).toHaveAttribute("data-stand", "needs_you");
});

test("an opened repository's commits page 5, 10 and 25 at a time, Newer and Older off at either end", async ({ page }) => {
  await page.goto("/ship");
  const kiln = repoRow(page);
  await kiln.getByRole("button", { name: "Open hyper-light/kiln", exact: true }).click();
  await expect(lines(kiln)).toHaveCount(10, { timeout: 15_000 });
  await expect(pressed(kiln, "10 a page")).toHaveAttribute("aria-pressed", "true");
  await expect(pressed(kiln, "Newer")).toBeDisabled();
  await expect(pressed(kiln, "Older")).toBeDisabled();
  // Its newest is picked until another is.
  await expect(lines(kiln).first()).toHaveAttribute("data-sel", "true");
  await expect(lines(kiln).first()).toContainText("4444444");

  await pressed(kiln, "5 a page").click();
  await expect(lines(kiln)).toHaveCount(5);
  await expect(kiln.locator(".sc-pager span")).toHaveText("1–5");
  await expect(pressed(kiln, "Newer")).toBeDisabled();
  await expect(pressed(kiln, "Older")).toBeEnabled();
  const first = await lines(kiln).allInnerTexts();
  await pressed(kiln, "Older").click();
  await expect(kiln.locator(".sc-pager span")).toHaveText("6–10");
  await expect(lines(kiln)).toHaveCount(5);
  expect(await lines(kiln).allInnerTexts()).not.toEqual(first);
  await expect(pressed(kiln, "Newer")).toBeEnabled();
  await expect(pressed(kiln, "Older")).toBeDisabled();
  await pressed(kiln, "Newer").click();
  await expect(kiln.locator(".sc-pager span")).toHaveText("1–5");

  // A page bigger than what it has: all of them, from the first.
  await pressed(kiln, "25 a page").click();
  await expect(lines(kiln)).toHaveCount(10);
  await expect(kiln.locator(".sc-pager span")).toHaveText("1–10");
  await expect(pressed(kiln, "Older")).toBeDisabled();
});

test("picking a commit puts its own Files, Conversation and Jobs under the divider; paging keeps it picked", async ({ page }) => {
  await page.goto("/ship");
  const kiln = await pickCommit(page, "3333333");
  const files = kiln.locator('section[aria-label="Files"]');
  await expect(files).toContainText("3333333", { timeout: 15_000 });
  await expect(kiln.locator('section[aria-label="Conversation"]')).toContainText("sync: a move carries its homes");
  await expect(kiln.locator('section[aria-label="Jobs"]')).toBeVisible();
  // The divider: a hairline under the commits, Files after it.
  const commits = kiln.locator("section.ship-commits");
  const edge = await commits.evaluate((el) => {
    const cs = getComputedStyle(el);
    return { width: cs.borderBottomWidth, style: cs.borderBottomStyle };
  });
  expect(edge).toEqual({ width: "1px", style: "solid" });
  const below = await commits.evaluate((el) => {
    const next = el.parentElement?.querySelector('section[aria-label="Files"]');
    return next ? next.getBoundingClientRect().top >= el.getBoundingClientRect().bottom : false;
  });
  expect(below).toBe(true);
  // Paging moves the list only: the pick stays, and so do its sections.
  await pressed(kiln, "5 a page").click();
  await expect(lines(kiln)).toHaveCount(5);
  await expect(files).toContainText("3333333");
  // Its newest again from the row's Review: the sections are the newest's.
  await kiln
    .getByRole("button", { name: /^Review/ })
    .first()
    .click();
  await expect(kiln.locator('section.ship-commits .sc-c[data-sel="true"]')).toContainText("4444444");
});

for (const width of [1280, 390, 320]) {
  test(`at ${width}, the commits hold their height as they page, and nothing scrolls sideways`, async ({ page, isMobile }) => {
    test.skip(isMobile, "each width is set here; the desktop projects run them all");
    await page.setViewportSize({ width, height: 900 });
    await page.goto("/ship");
    const kiln = repoRow(page);
    await kiln.getByRole("button", { name: "Open hyper-light/kiln", exact: true }).click();
    await expect(lines(kiln)).toHaveCount(10, { timeout: 15_000 });
    await pressed(kiln, "5 a page").click();
    await expect(lines(kiln)).toHaveCount(5);
    const list = kiln.locator("section.ship-commits .sc-list");
    const h1 = (await list.boundingBox())?.height ?? 0;
    await pressed(kiln, "Older").click();
    await expect(kiln.locator(".sc-pager span")).toHaveText("6–10");
    const h2 = (await list.boundingBox())?.height ?? 0;
    expect(Math.abs(h2 - h1)).toBeLessThanOrEqual(1);
    const sideways = await page.evaluate(() => {
      const s = document.scrollingElement;
      return s ? s.scrollWidth - s.clientWidth : 0;
    });
    expect(sideways).toBeLessThanOrEqual(0);
  });
}

test("on a phone each commit takes two lines: its message and when, then its sha, author, machine and run time", async ({ page, isMobile }) => {
  test.skip(!isMobile, "the phone projects run it");
  await page.goto("/ship");
  const kiln = repoRow(page);
  await kiln.getByRole("button", { name: "Open hyper-light/kiln", exact: true }).click();
  const one = lines(kiln).first();
  await expect(one).toBeVisible({ timeout: 15_000 });
  await expect(one.locator(".sc-l2")).toBeVisible();
  await expect(one.locator(".sc-l2")).toContainText("4444444");
  await expect(one.locator(".sc-who")).toBeHidden();
  await expect(one.locator(".sc-on")).toBeHidden();
});
