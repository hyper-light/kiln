/**
 * Ship's Hand off (docs/ci.md W26-R18, R02), as review-ship.html draws it, against the mock: a
 * person's failed commit offers Hand off first, then Retry; handed off, its failure is on the agent
 * the repository has attached (ci-doctor) and its row says so, with Retry alone. An agent's change
 * hands its open thread to its author's agent; its row then reads that agent at work, with no Hand
 * off and Review pair. The row holds its height throughout. What a browser hands off is its own
 * (the mock keeps it by user agent), reset before and after.
 */
import { expect, test, type Page } from "@playwright/test";

const REAL = !!process.env.E2E_BASE_URL;
test.skip(REAL, "drives the mock's commits");

const rowOf = (page: Page, text: string) => page.getByRole("list", { name: "Commits", exact: true }).locator(":scope > li", { hasText: text }).first();

test("a person's failure goes to the agent attached to it, and its row says who has it", async ({ page }) => {
  await page.request.post("/api/mock/ship/hand-off?reset=1");
  try {
    await page.goto("/ship");
    const row = rowOf(page, "sync: a move carries its homes");
    await expect(row).toContainText("rust lint + test · assertion failed in sync::reconnect");
    const acts = row.locator(".ship-acts > button");
    await expect(acts.nth(0)).toHaveText("Hand off");
    await expect(acts.nth(1)).toHaveText("Retry");
    const before = (await row.boundingBox())?.height ?? 0;

    await row.getByRole("button", { name: "Hand off" }).click();
    await expect(row).toContainText("ci-doctor fixing · assertion failed in sync::reconnect");
    await expect(row.getByRole("button", { name: "Hand off" })).toHaveCount(0);
    await expect(row.getByRole("button", { name: "Retry" })).toBeVisible();
    expect((await row.boundingBox())?.height ?? 0).toBe(before);
  } finally {
    await page.request.post("/api/mock/ship/hand-off?reset=1");
  }
});

test("a change's open thread goes to its author's agent, and its row reads that agent at work", async ({ page }) => {
  await page.request.post("/api/mock/ship/hand-off?reset=1");
  try {
    await page.goto("/ship");
    const row = rowOf(page, "pipeline: dynamic matrices");
    await expect(row).toContainText("kim: empty matrix meant? · 1 thread");
    const before = (await row.boundingBox())?.height ?? 0;

    await row.getByRole("button", { name: "Hand off" }).click();
    await expect(row).toContainText("matrix fixing · kim's thread");
    await expect(row.getByRole("button", { name: "Hand off" })).toHaveCount(0);
    await expect(row.getByRole("button", { name: "Review" })).toHaveCount(0);
    expect((await row.boundingBox())?.height ?? 0).toBe(before);
  } finally {
    await page.request.post("/api/mock/ship/hand-off?reset=1");
  }
});

test("asked again, Hand off says there's nothing open to hand", async ({ page }) => {
  await page.request.post("/api/mock/ship/hand-off?reset=1");
  try {
    const sha = "3".repeat(40);
    const first = await page.request.post("/api/ship/hand-off", { data: { repo: "hyper-light/kiln", sha } });
    expect(first.status()).toBe(200);
    expect(await first.json()).toMatchObject({ handed: { to: "ci-doctor", what: ["checks"] } });
    const again = await page.request.post("/api/ship/hand-off", { data: { repo: "hyper-light/kiln", sha } });
    expect(again.status()).toBe(409);
    expect(await again.json()).toMatchObject({ error: "3333333 has nothing open to hand off" });
  } finally {
    await page.request.post("/api/mock/ship/hand-off?reset=1");
  }
});
