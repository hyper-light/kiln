/**
 * An agent's Duplicate (its ⋯ menu, actions.tsx): Mission control's composer, filled in as the agent
 * runs (its harness, machine, folder, permissions and model), said to come from it and focused to
 * write the new one's first message. Against the mock, on a desktop and a phone.
 */
import { expect, test } from "@playwright/test";

const REAL = !!process.env.E2E_BASE_URL;
test.skip(REAL, "the mock's agents");

test("Duplicate opens Mission control's composer filled in as the agent runs", async ({ page }) => {
  await page.goto("/agents/demo-claude");
  await expect(page.locator(".hx-agent.ag-ready")).toBeVisible({ timeout: 15_000 });
  await page
    .getByRole("button", { name: /^More for / })
    .first()
    .click();
  await page.getByRole("menuitem", { name: "Duplicate" }).click();
  await expect(page).toHaveURL(/\/agents(\?.*)?$/);
  await expect(page.locator(".mc-from")).toContainText(/^Like /);
  await expect(page.getByRole("textbox", { name: "Its first message" })).toBeFocused();
});
