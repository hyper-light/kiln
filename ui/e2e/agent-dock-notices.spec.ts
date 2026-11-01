/**
 * The cards of other agents waiting on you (and kiln's toasts) don't cover an agent's message box
 * (agent.tsx, liftNotices; W21-R13). Where their column would cover it, at a phone's width or a
 * desktop's with the side tray put away, they sit above the dock, and a tap on the field reaches
 * it. Beside the open side tray they stay at the window's bottom. In the production build against
 * the mock (its other agents wait on you), on a desktop and a phone.
 */
import { expect, test, type Locator } from "@playwright/test";

/** Against a real kiln server (playwright.config.ts): these drive the mock's agents. */
const REAL = !!process.env.E2E_BASE_URL;

type Box = { x: number; y: number; width: number; height: number };

/** Whether two boxes share any area. */
const meet = (a: Box, b: Box) => a.x < b.x + b.width && b.x < a.x + a.width && a.y < b.y + b.height && b.y < a.y + a.height;

async function box(l: Locator): Promise<Box> {
  const b = await l.boundingBox();
  if (!b) throw new Error("not on the page");
  return b;
}

test("other agents' cards sit above an agent's message box, not over it (W21-R13)", async ({ page, isMobile }) => {
  test.skip(REAL, "the mock's agents");
  // Their cards back, whatever the checks before put away (mock/push.ts, forgetMarks).
  await page.request.post("/api/mock/asks");
  await page.goto("/agents/demo-claude");
  // The page once it's whole: the box drawn while its conversation loads takes what's written too (W21-R15).
  await expect(page.locator(".hx-agent.ag-ready")).toBeVisible();
  const field = page.getByRole("textbox", { name: "Message to the agent" });
  await expect(field).toBeEnabled();
  const cards = page.getByRole("list", { name: "Waiting on you" });
  await expect(cards.getByRole("listitem").first()).toBeVisible();
  const dock = page.locator(".hx-dock");
  const composer = dock.locator(".hx-composer");
  if (!isMobile) {
    // Beside the open side tray they cover nothing of it: they stay at the window's bottom, under the dock's top.
    await expect.poll(async () => (await box(cards)).y + (await box(cards)).height > (await box(dock)).y).toBe(true);
    expect(meet(await box(cards), await box(composer))).toBe(false);
    await page.getByRole("button", { name: "Collapse side tray" }).click();
  }
  // Above the dock, clear of the message box.
  await expect.poll(async () => (await box(cards)).y + (await box(cards)).height <= (await box(dock)).y).toBe(true);
  expect(meet(await box(cards), await box(composer))).toBe(false);
  // A tap on the field is the field's.
  await field.click();
  await expect(field).toBeFocused();
});
