/**
 * Closing find on an agent's page gives its composer's field the focus back (agent.tsx), by Escape
 * or by its ×: the field itself, not the marked copy drawn under it (compose-field.tsx), which has
 * its class too. In the production build against the mock, on a desktop and a phone.
 */
import { expect, test } from "@playwright/test";

/** Against a real kiln server (playwright.config.ts): these drive the mock's agents. */
const REAL = !!process.env.E2E_BASE_URL;

test("closing find gives the composer's field the focus back", async ({ page }) => {
  test.skip(REAL, "the mock's agents");
  await page.goto("/agents/demo-claude");
  // The agent's own composer, not the one drawn while its conversation loads (the same name, turned off).
  const field = page.getByRole("textbox", { name: "Message to the agent" });
  await expect(field).toBeEnabled();
  const find = page.getByRole("textbox", { name: "Find in this conversation" });
  for (const close of ["Escape", "its ×"] as const) {
    await page.keyboard.press("ControlOrMeta+f");
    await expect(find).toBeFocused();
    await find.fill("test");
    if (close === "Escape") await find.press("Escape");
    else await page.getByRole("button", { name: "Close find" }).click();
    await expect(find).toBeHidden();
    await expect(field, `closed by ${close}`).toBeFocused();
  }
});
