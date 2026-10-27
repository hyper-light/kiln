/**
 * The opened item's sections in their order (W26-R06, R14): Files (with the Conversation), the
 * terminal output, Jobs right above Lanes — for every item at every stage: a commit with no
 * change, a diff, a change request and a landed change. Chromium and WebKit, desktop and phone.
 */
import { expect, test, type Page } from "@playwright/test";

const REAL = !!process.env.E2E_BASE_URL;
test.skip(REAL, "drives the mock's commits");

const SECTIONS = [/^Files/, "Conversation", "Output", "Jobs", "Lanes"];

const open = async (page: Page, name: string) => {
  await page.goto("/ship");
  const row = page.locator(".ship-row", { hasText: name }).first();
  await row.locator(".ship-main").click();
  return row;
};

test("a commit with no change opens Files, Conversation, Output, Jobs, Lanes", async ({ page }) => {
  const row = await open(page, "sync: a move carries its homes");
  await expect(row.locator(".rv-s > h3, .ship-sec > h3")).toHaveText(SECTIONS);
});

test("a diff opens Files, Conversation, Output, Jobs, Lanes", async ({ page }) => {
  const row = await open(page, "pipeline: dynamic matrices");
  await expect(row.locator(".rv-s > h3, .ship-sec > h3")).toHaveText(SECTIONS);
});

test("a change request opens Files, Conversation, Output, Jobs, Lanes", async ({ page }) => {
  const row = await open(page, "sched: fair shares");
  await expect(row.locator(".rv-s > h3, .ship-sec > h3")).toHaveText(SECTIONS);
});

test("a landed change opens Files, Conversation, Output, Jobs, Lanes", async ({ page }) => {
  const row = await open(page, "ci: a landed change");
  await expect(row.locator(".rv-s > h3, .ship-sec > h3")).toHaveText(SECTIONS);
});
