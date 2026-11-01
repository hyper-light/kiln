/**
 * Ship's Hand off (docs/ci.md W26-R18, R02), as review-ship.html draws it, against the mock: a
 * person's failed commit offers Hand off first, then Retry; handed off, its failure is on the agent
 * the repository has attached (ci-doctor) and its row says so, with Retry alone. An agent's change
 * hands its open thread to its author's agent; its row then reads that agent at work, with no Hand
 * off and Review pair. An agent's stacked diff and change request alike hand both their threads
 * over, read fixing, and once the agent's fix is in, the handed revision says "Fixed in v2", folded
 * under v2. The row holds its height throughout. What a browser hands off is its own (the mock
 * keeps it by user agent), reset before and after.
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
    // Handed off from this browser, which has no key: kiln signed it for them, and says so (W27-R08).
    await expect(page.getByText("kiln signed the hand-off for you: this browser has no key.")).toBeVisible();
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

/** A row's own line (its head), whose height holds while what it says changes. */
const headHeight = async (row: ReturnType<typeof rowOf>) => (await row.locator(":scope > .ship-r").boundingBox())?.height ?? 0;

for (const [kind, name, title, sha] of [
  ["diff", "D45", "matrix: cap the expansion", "d5".repeat(20)],
  ["request", "CR15", "sched: cap a share", "c5".repeat(20)],
] as const) {
  test(`w26_r18 an agent's ${kind === "diff" ? "stacked diff" : "change request"} hands both its threads to its agent, reads fixing, then fixed in v2`, async ({ page }) => {
    await page.request.post("/api/mock/ship/hand-off?reset=1");
    await page.request.post(`/api/mock/ship/arrive?agent=${kind}`);
    try {
      await page.goto("/ship");
      const row = rowOf(page, title);
      await expect(row).toContainText(name);
      await expect(row).toContainText("kim: what caps it? · 2 threads");
      const before = await headHeight(row);

      await row.getByRole("button", { name: "Hand off" }).click();
      await expect(row).toContainText("matrix fixing · 2 threads");
      await expect(row.getByRole("button", { name: "Hand off" })).toHaveCount(0);
      expect(await headHeight(row)).toBe(before);

      // The agent's fix is in: v2 on top, the handed revision folded under it, saying which version fixed it.
      expect((await page.request.post(`/api/mock/ship/fixed?sha=${sha}`)).status()).toBe(200);
      await page.reload();
      const tip = rowOf(page, title);
      await expect(tip).toContainText("Checks running");
      expect(await headHeight(tip)).toBe(before);
      await tip.getByRole("button", { name: new RegExp(`Show 1 more commit of ${name}`) }).click();
      await expect(tip.getByRole("list", { name: `The other commits of ${name}` })).toContainText("Fixed in v2");
    } finally {
      await page.request.post("/api/mock/ship/hand-off?reset=1");
      await page.request.post("/api/mock/ship/arrive?clear=1");
    }
  });
}

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
