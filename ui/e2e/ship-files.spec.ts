/**
 * An item's diff in its one place (Changes plan 01a17bd748ff73d98f09996ecdc6873d, step 2; Ada,
 * 2026-10-27: "One place", Review's Files first in the dropdown), against the mock's D41: each file
 * shows its status, its +/- and Viewed; marking one Viewed folds it, and it stays marked and folded
 * on reload; the Files section is the lanes card's width within 1 px at 1280 and 390, and from 320 to
 * 1440 px for a commit, a change and a host's pull request (Opened commit plan
 * 01a175b7f21b7455bcdcd71e94ecdce0, step 1, Files).
 */
import { expect, test, type Page } from "@playwright/test";

const REAL = !!process.env.E2E_BASE_URL;
test.skip(REAL, "drives the mock's review");

/** D41's row opened, its Files drawn. */
const openD41 = async (page: Page) => {
  await page.goto("/ship");
  const row = page.locator(".ship-row").filter({ hasText: "D41" }).first();
  await expect(row).toBeVisible({ timeout: 15_000 });
  await row.locator(".ship-go").click();
  const files = row.locator('section[aria-label="Files"]');
  await expect(files.locator(".rv-sum")).toBeVisible({ timeout: 15_000 });
  return { row, files, each: files.locator("details.rv-f") };
};

for (const width of [1280, 390])
  test(`at ${width}, each file shows its status, +/- and Viewed; Viewed folds it and holds on reload; Files is the lanes card's width`, async ({ page, isMobile }) => {
    test.skip(isMobile, "the width is set here; the desktop projects run it");
    test.setTimeout(90_000);
    await page.setViewportSize({ width, height: 900 });
    let marked = "";
    try {
      const { row, files, each } = await openD41(page);
      const n = await each.count();
      expect(n).toBeGreaterThan(0);
      for (let i = 0; i < n; i++) {
        const f = each.nth(i);
        await expect(f.locator("summary .rv-chip").first()).toHaveText(/^(added|deleted|modified|renamed|copied|type changed)/);
        await expect(f.locator("summary .rv-add")).toHaveText(/^\+\d+$/);
        await expect(f.locator("summary .rv-del")).toHaveText(/^−\d+$/);
        await expect(f.getByRole("checkbox", { name: "Viewed" })).toBeVisible();
      }
      // The Files section and the lanes card (the Lanes section, a card as Output and Jobs are), edge to edge, within 1 px.
      const card = row.locator('section[aria-label="Lanes"]');
      await expect(card.locator(".ship-lanesbox")).toBeVisible({ timeout: 15_000 });
      const w = { files: (await files.boundingBox())?.width ?? 0, lanes: (await card.boundingBox())?.width ?? 0 };
      expect(Math.abs(w.files - w.lanes), `Files ${w.files} px, the lanes card ${w.lanes} px`).toBeLessThanOrEqual(1);
      // Viewed folds the file...
      const first = each.first();
      marked = (await first.locator("summary .rv-path").textContent()) ?? "";
      expect(marked).not.toBe("");
      await expect(first).toHaveJSProperty("open", true);
      await first.getByRole("checkbox", { name: "Viewed" }).check();
      await expect(first).toHaveJSProperty("open", false);
      await expect(first.getByRole("checkbox", { name: "Viewed" })).toBeChecked();
      // ...and after a reload it's still marked, and folded; unticked, it opens again.
      await expect
        .poll(async () => ((await (await page.request.get("/api/diffs/D41/versions/7/files")).json()) as { files: { path: string; viewed: boolean }[] }).files.find((f) => f.path === marked)?.viewed)
        .toBe(true);
      await page.reload();
      const again = await openD41(page);
      const same = again.each.filter({ has: page.locator("summary .rv-path", { hasText: marked }) }).first();
      await expect(same.getByRole("checkbox", { name: "Viewed" })).toBeChecked();
      await expect(same).toHaveJSProperty("open", false);
      await same.getByRole("checkbox", { name: "Viewed" }).uncheck();
      await expect(same).toHaveJSProperty("open", true);
    } finally {
      if (marked) await page.request.post("/api/diffs/D41/viewed", { data: { file: marked, viewed: false } });
    }
  });

test("from 320 to 1440 px, every item's Files is the lanes card's width within 1 px", async ({ page, isMobile }) => {
  test.skip(isMobile, "the widths are set here; the desktop projects run them");
  test.setTimeout(120_000);
  // A kiln change (D41), a host's pull request (!6, GitLab) and a commit with no change (sync).
  for (const name of ["D41", "ci: a landed change", "sync: a move carries its homes"])
    for (const width of [320, 390, 600, 800, 1000, 1280, 1440]) {
      await page.setViewportSize({ width, height: 900 });
      await page.goto("/ship");
      const row = page.locator(".ship-row").filter({ hasText: name }).first();
      await expect(row).toBeVisible({ timeout: 15_000 });
      await row.locator(".ship-go").click();
      const files = row.locator('section[aria-label="Files"]');
      const card = row.locator('section[aria-label="Lanes"]');
      await expect(files.locator(".rv-sum")).toBeVisible({ timeout: 15_000 });
      await expect(card.locator(".ship-lanesbox")).toBeVisible({ timeout: 15_000 });
      const w = { files: (await files.boundingBox())?.width ?? 0, lanes: (await card.boundingBox())?.width ?? 0 };
      expect(Math.abs(w.files - w.lanes), `${name} at ${width}: Files ${w.files} px, the lanes card ${w.lanes} px`).toBeLessThanOrEqual(1);
    }
});
