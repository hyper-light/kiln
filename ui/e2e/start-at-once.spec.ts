/**
 * W21-R15: Start never waits for the server. Pressed, Mission control opens the new agent's page at
 * once, under the id the page made (the server starts it under that id), its first message there
 * as sent and the agent said to be starting, while the start is still on its way (held here for
 * 2 s); the conversation follows once it has started, the address never changing. A start the
 * server refuses says why on that page: Try again starts it under the same id, Back puts its
 * message back in Mission control's box. Against the mock, on a desktop and a phone.
 */
import { expect, test, type Page } from "@playwright/test";

const REAL = !!process.env.E2E_BASE_URL;
test.skip(REAL, "starts the mock's agents");

/** Holds each start `ms` on its way (none: as the mock answers), or answers the first `refuse` of them with that. */
async function holdStarts(page: Page, ms: number, refuse?: { status: number; error: string }) {
  let refused = false;
  await page.route("**/api/harness/sessions", async (route) => {
    if (route.request().method() !== "POST") return route.continue();
    await new Promise((r) => setTimeout(r, ms));
    if (refuse && !refused) {
      refused = true;
      return route.fulfill({ status: refuse.status, contentType: "application/json", body: JSON.stringify({ error: refuse.error }) });
    }
    return route.continue();
  });
}

async function write(page: Page, text: string) {
  await page.goto("/agents");
  const box = page.getByRole("textbox", { name: "Its first message" });
  await expect(box).toBeVisible();
  await box.fill(text);
  // Start knows the machines' answer once it can start anything.
  const start = page.locator(".mc-start");
  await expect(start).toBeEnabled({ timeout: 15_000 });
  return start;
}

test("w21_r15 Start opens the new agent's page at once, its message there, and its conversation follows under the same address", async ({ page }) => {
  await holdStarts(page, 2000);
  const said = `Find why the retry test is flaky (${Date.now()})`;
  const start = await write(page, said);
  const t0 = Date.now();
  await start.click();
  // At once: its page, under an id made here, with what was asked and that it's starting.
  await expect(page).toHaveURL(/\/agents\/[0-9a-f]{32}$/, { timeout: 1000 });
  const opened = Date.now() - t0;
  const timeline = page.locator(".hx-timeline");
  await expect(timeline.locator(".hx-sending .hx-prompt-text")).toHaveText(said, { timeout: 1000 });
  await expect(page.locator(".hx-dock").getByText(/^Starting Claude Code on /)).toBeVisible({ timeout: 1000 });
  expect(opened, "its page opened while the start was still on its way").toBeLessThan(1500);
  const address = page.url();
  // Its message box takes what's written meanwhile, kept to go once it has started.
  const box = page.getByRole("textbox", { name: "Message to the agent" });
  await box.fill("And tell me what you changed");
  await page.getByRole("button", { name: "Queue" }).click();
  await expect(timeline.getByText("And tell me what you changed")).toBeVisible();
  // Started: its conversation, under the same address.
  await expect(page.locator(".hx-agent.ag-ready")).toBeVisible({ timeout: 15_000 });
  await expect(page.locator(".hx-prompt-text").filter({ hasText: said }).first()).toBeVisible();
  expect(page.url()).toBe(address);
});

test("w21_r15 a start the server refuses says why on its page, and Back puts its message back to start it from there", async ({ page }) => {
  await holdStarts(page, 300, { status: 400, error: "machine metal-ryzen-01 is not connected" });
  const said = `Trim the release notes (${Date.now()})`;
  await (await write(page, said)).click();
  await expect(page).toHaveURL(/\/agents\/[0-9a-f]{32}$/, { timeout: 1000 });
  const failed = page.locator(".hx-startfail");
  await expect(failed).toContainText("machine metal-ryzen-01 is not connected");
  await expect(page.locator(".hx-sending .hx-delivery")).toHaveText("not sent");
  // Back: Mission control's box has its message again.
  await failed.getByRole("button", { name: "Back to Mission control" }).click();
  await expect(page).toHaveURL(/\/agents$/);
  await expect(page.getByRole("textbox", { name: "Its first message" })).toHaveValue(said);
  // Started again from there, refused no more: it starts.
  await (await write(page, said)).click();
  await expect(page.locator(".hx-agent.ag-ready")).toBeVisible({ timeout: 15_000 });
});

test("w21_r15 Try again starts a refused agent under the same id, its page staying where it is", async ({ page }) => {
  await holdStarts(page, 300, { status: 400, error: "machine metal-ryzen-01 is not connected" });
  await (await write(page, `Summarize the scheduler (${Date.now()})`)).click();
  await expect(page.locator(".hx-startfail")).toBeVisible({ timeout: 5000 });
  const address = page.url();
  await page.getByRole("button", { name: "Try again" }).click();
  // At once: starting again.
  await expect(page.locator(".hx-startfail")).toHaveCount(0, { timeout: 1000 });
  await expect(page.locator(".hx-dock").getByText(/^Starting Claude Code on /)).toBeVisible({ timeout: 1000 });
  await expect(page.locator(".hx-agent.ag-ready")).toBeVisible({ timeout: 15_000 });
  expect(page.url()).toBe(address);
});
