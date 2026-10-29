// W24-R31: the reaper's policies in the UI, at each scope: what's in force and where each value came
// from, and the scope's own keys edited in place (docs/ci.md W24-R31).
import { expect, test } from "@playwright/test";

test("the system page shows and edits the system's and a runner's cache policy", async ({ page }) => {
  await page.goto("/system#system.reaper");
  const system = page.getByRole("region", { name: "The system's policy" });
  await expect(system.getByText("102.4 GiB · the system")).toBeVisible();
  await system.getByLabel("Keep the newest").fill("3");
  await system.getByRole("button", { name: "Save" }).click();
  await expect(system.getByText("3 per lane · the system")).toBeVisible();
  await expect(system.getByText(/Set by /)).toBeVisible();
  const runner = page.getByRole("region", { name: "A runner's policy" });
  await runner.getByLabel("A runner").selectOption("metal-ryzen-01");
  await expect(runner.getByText("200 GiB · the runner")).toBeVisible();
  await expect(runner.getByText("3 per lane · the system")).toBeVisible();
});

test("a repository's details show and edit its cache policy, without hardware limits", async ({ page }) => {
  await page.request.post("/api/repos", { data: { name: "acme/reaped" } });
  await page.goto("/repos");
  await page.getByText("acme/reaped").first().click();
  const repo = page.getByRole("region", { name: "What its caches keep" });
  await expect(repo.getByLabel("Floor")).toHaveCount(0);
  await repo.getByLabel("Drop what's unused for").fill("14d");
  await repo.getByRole("button", { name: "Save" }).click();
  await expect(repo.getByText("14d · the repository")).toBeVisible();
});

test("an agent's settings show and edit its session's cache policy", async ({ page }) => {
  await page.goto("/agents/sub-busy");
  await page.getByRole("button", { name: /^More for / }).first().click();
  await page.getByRole("menuitem", { name: "Settings…" }).click();
  const session = page.getByRole("region", { name: "What its caches keep" });
  await session.getByLabel("Schedule").fill("6h");
  await session.getByRole("button", { name: "Save" }).click();
  await expect(session.getByText("6h · the session")).toBeVisible();
});
