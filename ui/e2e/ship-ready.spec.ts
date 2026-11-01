/**
 * Ship's Ready (docs/ci.md W26-R18, drafts): an agent's draft, a change request on kiln or a pull
 * request on GitHub, offers Ready on its row; its person presses it and the row reads waiting on
 * review, Ready gone, the row's line at its height. What a browser marks ready is its own (the
 * mock keeps it by user agent), reset before and after.
 */
import { expect, test, type Page } from "@playwright/test";

const REAL = !!process.env.E2E_BASE_URL;
test.skip(REAL, "drives the mock's commits");

const rowOf = (page: Page, text: string) => page.getByRole("list", { name: "Commits", exact: true }).locator(":scope > li", { hasText: text }).first();

/** A row's own line (its head), whose height holds while what it says changes. */
const headHeight = async (row: ReturnType<typeof rowOf>) => (await row.locator(":scope > .ship-r").boundingBox())?.height ?? 0;

for (const [host, name, title] of [
  ["kiln", "CR16", "matrix: draft a share"],
  ["github", "#22", "matrix: draft on GitHub"],
] as const) {
  test(`w26_r18 an agent's draft on ${host} is marked ready from its row, which then reads waiting on review`, async ({ page }) => {
    await page.request.post("/api/mock/ship/ready?reset=1");
    await page.request.post(`/api/mock/ship/arrive?draft=${host}`);
    try {
      await page.goto("/ship");
      const row = rowOf(page, title);
      await expect(row).toContainText(name);
      await expect(row).toContainText("Draft · not asked for review yet");
      const before = await headHeight(row);

      await row.getByRole("button", { name: "Ready" }).click();
      await expect(row).toContainText("Waiting on review");
      await expect(row.getByRole("button", { name: "Ready" })).toHaveCount(0);
      expect(await headHeight(row)).toBe(before);
    } finally {
      await page.request.post("/api/mock/ship/ready?reset=1");
      await page.request.post("/api/mock/ship/arrive?clear=1");
    }
  });
}
