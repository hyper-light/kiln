/** Ship's first review read starts its landing readiness alongside the other reads (W26-R15).
 * Hold the diff response so a sequential read cannot pass; the dropdown still waits for Files
 * before showing its sections, and preloading only reads data, never approving or landing it. */
import { expect, test } from "@playwright/test";

const REAL = !!process.env.E2E_BASE_URL;
test.skip(REAL, "holds the mock's synthetic review responses");

test("w26_r15 landing readiness starts before a held review diff has loaded", async ({ page }) => {
  let release = () => {};
  const held = new Promise<void>((done) => (release = done));
  let diffStarted = false;
  let readinessStarted = false;
  const mutations: string[] = [];
  page.on("request", (request) => {
    const path = new URL(request.url()).pathname;
    if (path === "/api/diffs/D41/land" && request.method() === "GET") readinessStarted = true;
    if (path.startsWith("/api/diffs/") && request.method() !== "GET") mutations.push(`${request.method()} ${path}`);
  });
  await page.route("**/api/diffs/D41", async (route) => {
    diffStarted = true;
    await held;
    await route.continue();
  });
  try {
    await page.goto("/ship?repo=hyper-light%2Fkiln");
    const row = page.getByRole("list", { name: "Repositories", exact: true }).locator(":scope > li").first();
    await expect(row).toContainText("pipeline: dynamic matrices", { timeout: 30_000 });
    await row.getByRole("button", { name: "Open hyper-light/kiln", exact: true }).click();
    await expect.poll(() => diffStarted, { timeout: 30_000 }).toBe(true);
    await expect.poll(() => readinessStarted, { timeout: 30_000 }).toBe(true);
    // Its single review boundary still prevents a partially loaded dropdown from shifting Files.
    await expect(row.locator('section[aria-label="Files"]')).toHaveCount(0);
    await expect(row.getByRole("region", { name: "Jobs", exact: true })).toHaveCount(0);
    release();
    await expect(row.locator('section[aria-label="Files"]')).toBeVisible({ timeout: 30_000 });
    await expect(row.locator(".rv-s > h3, .ship-sech > h3")).toHaveText(["Commits", /^Files/, "Conversation", "Jobs", "Lanes"]);
    expect(mutations).toEqual([]);
  } finally {
    release();
    await page.unroute("**/api/diffs/D41");
  }
});

test("w26_r15 closed Ship rows defer section code, keyboard intent preloads it, and opening keeps its commits slot", async ({ page }) => {
  let release = () => {};
  const held = new Promise<void>((done) => (release = done));
  let codeStarted = false;
  const writes: string[] = [];
  page.on("request", (request) => {
    const path = new URL(request.url()).pathname;
    if ((path.startsWith("/api/diffs/") || path.startsWith("/api/ship/")) && request.method() !== "GET") writes.push(`${request.method()} ${path}`);
  });
  await page.route("**/assets/sections-*.js", async (route) => {
    codeStarted = true;
    await held;
    await route.continue();
  });
  try {
    await page.goto("/ship?repo=hyper-light%2Fkiln");
    const row = page.getByRole("list", { name: "Repositories", exact: true }).locator(":scope > li").first();
    await expect(row).toContainText("pipeline: dynamic matrices", { timeout: 30_000 });
    expect(codeStarted).toBe(false);
    await expect(row.locator(".ship-open")).toHaveCount(0);
    const open = row.getByRole("button", { name: "Open hyper-light/kiln", exact: true });
    await open.focus();
    await expect.poll(() => codeStarted, { timeout: 30_000 }).toBe(true);
    // Reading a chunk and its data on intent does not mount a hidden dropdown or run an action.
    await expect(open).toHaveAttribute("aria-expanded", "false");
    await expect(row.locator(".ship-open")).toHaveCount(0);
    await open.press("Enter");
    await expect(row.locator("section.ship-commits")).toBeVisible({ timeout: 30_000 });
    await expect(row.locator('section[aria-label="Files"]')).toHaveCount(0);
    await expect(row.getByRole("status").filter({ hasText: "Loading its review" })).toBeVisible();
    release();
    await expect(row.locator('section[aria-label="Files"]')).toBeVisible({ timeout: 30_000 });
    await expect(row.locator(".rv-s > h3, .ship-sech > h3")).toHaveText(["Commits", /^Files/, "Conversation", "Jobs", "Lanes"]);
    expect(writes).toEqual([]);
  } finally {
    release();
    await page.unroute("**/assets/sections-*.js");
  }
});
