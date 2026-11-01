/**
 * Phones don't zoom into a field as it takes the focus (routes/__root.tsx, W21-R13): on a touch
 * device the page's one viewport tag says maximum-scale=1 once React has loaded, and still does
 * after a move to another page. Browsers follow the last viewport tag, so a plain one added after
 * it (React did, as it took over the page) undoes it. A desktop's tag stays plain. In the
 * production build against the mock, on a desktop and a phone.
 */
import { expect, test } from "@playwright/test";

/** Against a real kiln server (playwright.config.ts): these drive the mock's agents. */
const REAL = !!process.env.E2E_BASE_URL;

const VIEWPORT = "width=device-width, initial-scale=1, viewport-fit=cover";

const viewports = () => [...document.querySelectorAll('meta[name="viewport"]')].map((m) => m.getAttribute("content"));

test("a phone's one viewport tag says not to zoom into fields, once the page has loaded and from page to page (W21-R13)", async ({ page, hasTouch }) => {
  test.skip(REAL, "the mock's agents");
  const expected = [hasTouch ? `${VIEWPORT}, maximum-scale=1` : VIEWPORT];
  await page.goto("/agents/demo-claude");
  // Loaded: the agent's own composer, not the one drawn while its conversation loads (turned off).
  await expect(page.getByRole("textbox", { name: "Message to the agent" })).toBeEnabled();
  expect(await page.evaluate(viewports)).toEqual(expected);
  // To the overview without a page load: its head's tags are rendered again.
  await page.evaluate(() => Object.assign(window, { stayed: true }));
  await page.getByRole("link", { name: "kiln overview" }).click();
  await expect(page.getByRole("heading", { name: "Recent runs" })).toBeVisible();
  expect(await page.evaluate(() => "stayed" in window), "moved without a page load").toBe(true);
  expect(await page.evaluate(viewports)).toEqual(expected);
});
