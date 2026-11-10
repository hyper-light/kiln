/** W21-R02: inactive inspector code loads on intent, without changing the current tab or frame. */
import { expect, test, type Page } from "@playwright/test";

const REAL = !!process.env.E2E_BASE_URL;

async function holdTabs(page: Page) {
  let requests = 0;
  let release = () => {};
  const waiting = new Promise<void>((resolve) => (release = resolve));
  await page.route(
    (url) => /\/assets\/inspector-tabs[^/]*\.js$/.test(url.pathname),
    async (route) => {
      requests++;
      await waiting;
      await route.continue();
    },
  );
  return { requests: () => requests, release };
}

test.beforeEach(() => {
  test.skip(REAL, "controlled component-code delivery for the mock's sessions");
});

test("W21-R02 Plan intent loads code without changing Changes, its dimensions or the retained draft", async ({ page }) => {
  const held = await holdTabs(page);
  const errors: string[] = [];
  page.on("pageerror", (error) => errors.push(error.message));
  const draft = "Keep this draft while its inspector code loads.";
  await page.addInitScript((text) => sessionStorage.setItem("kiln.agents.drafts", JSON.stringify({ state: { drafts: { "tree-lead": text } }, version: 0 })), draft);
  try {
    await page.goto("/agents/tree-lead");
    const field = page.getByRole("textbox", { name: "Message to the agent", exact: true });
    await expect(field).toBeEnabled({ timeout: 30_000 });
    await expect(field).toHaveValue(draft);
    const changes = page
      .getByRole("tab", { name: /^Changes/ })
      .filter({ visible: true })
      .first();
    await changes.click();
    await expect(changes).toHaveAttribute("aria-selected", "true");
    expect(held.requests(), "the first Changes view does not import inactive tabs").toBe(0);
    const panel = page.locator(".hx-inspector-body").filter({ visible: true }).first();
    const before = await panel.boundingBox();
    expect(before).not.toBeNull();
    const plan = page.getByRole("tab", { name: /^Plan/ }).filter({ visible: true }).first();
    await plan.focus();
    await expect.poll(held.requests).toBe(1);
    await expect(changes).toHaveAttribute("aria-selected", "true", { timeout: 2000 });
    await expect(panel.locator(".in-secs")).toHaveCount(0);
    await plan.click();
    await expect(plan).toHaveAttribute("aria-selected", "true");
    await expect(panel.getByRole("status")).toContainText("Loading the plan");
    const during = await panel.boundingBox();
    expect(during).not.toBeNull();
    for (const key of ["x", "y", "width", "height"] as const) expect(Math.abs(during![key] - before![key]), key).toBeLessThanOrEqual(1);
    held.release();
    await expect(panel.locator(".in-secs")).toBeVisible();
    await expect(panel.getByRole("status").filter({ hasText: "Loading the plan" })).toHaveCount(0);
    const work = page.getByRole("tab", { name: /^Work/ }).filter({ visible: true });
    if (await work.count()) await work.first().click();
    await expect(field).toHaveValue(draft);
    expect(held.requests(), "focus and opening share the same module request").toBe(1);
    expect(errors).toEqual([]);
  } finally {
    held.release();
  }
});

test("W21-R02 a direct Plan link loads the real review in its inspector without a plain-text stand-in", async ({ page }) => {
  const held = await holdTabs(page);
  const errors: string[] = [];
  page.on("pageerror", (error) => errors.push(error.message));
  try {
    await page.goto("/agents/rev-cicd?tab=plan");
    await expect.poll(held.requests, { timeout: 30_000 }).toBe(1);
    const plan = page.getByRole("tab", { name: /^Plan/ }).filter({ visible: true }).first();
    await expect(plan).toHaveAttribute("aria-selected", "true");
    const panel = page.locator(".hx-inspector-body").filter({ visible: true }).first();
    await expect(panel.getByRole("status")).toContainText("Loading the plan");
    await expect(panel.locator(".pr")).toHaveCount(0);
    held.release();
    await expect(panel.locator(".pr .pr-eye")).toHaveText(/Needs you · a plan to review/);
    await expect(panel.locator(".pr .pr-step")).toHaveCount(5);
    expect(errors).toEqual([]);
  } finally {
    held.release();
  }
});

test("W21-R02 opening Tools loads its real controls and keeps a section's existing folded state", async ({ page }) => {
  const held = await holdTabs(page);
  try {
    await page.goto("/agents/tree-lead");
    await expect(page.getByRole("textbox", { name: "Message to the agent", exact: true })).toBeEnabled({ timeout: 30_000 });
    const tools = page
      .getByRole("tab", { name: /^Tools/ })
      .filter({ visible: true })
      .first();
    await tools.click();
    await expect.poll(held.requests).toBe(1);
    const panel = page.locator(".hx-inspector-body").filter({ visible: true }).first();
    await expect(panel.getByRole("status")).toContainText("Loading its tools");
    held.release();
    const usage = panel.getByRole("button", { name: "Usage and status", exact: true });
    await expect(usage).toHaveAttribute("aria-expanded", "true");
    await usage.click();
    await expect(usage).toHaveAttribute("aria-expanded", "false");
    await page.getByRole("tab", { name: /^Plan/ }).filter({ visible: true }).first().click();
    await expect(panel.locator(".in-secs")).toBeVisible();
    await tools.click();
    await expect(usage).toHaveAttribute("aria-expanded", "false");
    expect(held.requests()).toBe(1);
  } finally {
    held.release();
  }
});
