/**
 * W21-R13: viewport fitting keeps an agent's draft and focus, then lets an ordinary page scroll.
 * This changes browser viewport geometry; native phone keyboard/zoom behavior needs device checks.
 */
import { expect, test } from "@playwright/test";

test("W21-R13 changing viewport height keeps the composer and leaving restores ordinary page geometry", async ({ page }) => {
  test.skip(!!process.env.E2E_BASE_URL, "the mock's agent and overview");
  await page.goto("/agents/demo-claude");
  const field = page.getByRole("textbox", { name: "Message to the agent" });
  await expect(field).toBeEnabled();
  await field.fill("A draft kept while the window changes.");
  await field.focus();
  const node = await field.elementHandle();
  const width = page.viewportSize()?.width ?? 1280;
  for (const height of [480, 760, 520]) {
    await page.setViewportSize({ width, height });
    await expect
      .poll(() =>
        page.evaluate(() => {
          const view = window.visualViewport;
          const expected = Math.round(view ? view.height * view.scale : window.innerHeight);
          const root = document.documentElement;
          return root.classList.contains("fills-window") && root.style.getPropertyValue("--window-h") === `${expected}px` && (root.dataset.window === "short") === expected < 560;
        }),
      )
      .toBe(true);
    await expect(field).toBeFocused();
    await expect(field).toHaveValue("A draft kept while the window changes.");
    expect(await field.evaluate((el, original) => el === original, node), "fitting does not replace the field").toBe(true);
  }
  await page.getByRole("link", { name: "kiln overview" }).click();
  await expect(page.getByRole("heading", { name: "Recent runs" })).toBeVisible();
  await expect(page.locator("html.fills-window")).toHaveCount(0);
  expect(await page.evaluate(() => document.documentElement.style.getPropertyValue("--window-h"))).toBe("");
  expect(await page.evaluate(() => document.documentElement.dataset.window)).toBeUndefined();
});
