import { expect, test } from "@playwright/test";

test.skip(!!process.env.E2E_BASE_URL, "uses a disposable invitation on the mock server");

test("a fleet owner can request a one-machine session invite and return to runner enrollment", async ({ page }) => {
  let issued = 0;
  page.on("request", (request) => {
    if (new URL(request.url()).pathname === "/api/agents/session-node-invite") {
      expect(request.method()).toBe("POST");
      issued++;
    }
  });
  await page.goto("/runners/add");
  const issue = page.getByRole("button", { name: "Create a one-machine session-node invite" });
  await expect(issue).toBeVisible();
  expect(issued).toBe(0);
  await issue.click();
  await expect(page.locator(".am-cmd-text")).toContainText("session-invite.mock.");
  await expect(page.locator(".am-install")).toContainText("People authorized on this machine");
  await expect(page.locator(".am-install")).toContainText("Its token expires");
  await page.getByRole("button", { name: "Use runner command" }).click();
  await expect(issue).toBeVisible();
  await expect(page.locator(".am-cmd-text")).not.toContainText("session-invite.mock.");
  expect(issued).toBe(1);
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
});
