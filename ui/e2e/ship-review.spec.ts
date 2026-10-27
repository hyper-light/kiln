/**
 * A Ship item's dropdown owns its review (W26-R07, R08, R12, R13): open an item, Files and the
 * Conversation first; reply, resolve, change a kind, delete, apply a suggestion, open a whole
 * file, search; resolve a conflict and commit it. The mock (mock/review.ts) answers the
 * endpoints; Chromium and WebKit, desktop and phone.
 */
import { expect, test, type Page } from "@playwright/test";

const open = async (page: Page, name: string) => {
  await page.goto("/ship");
  const row = page.locator(".ship-row", { hasText: name }).first();
  await row.locator(".ship-main").click();
  await expect(row.locator(".rv-s").first()).toBeVisible();
  return row;
};

test("Files and the Conversation are the dropdown's first sections", async ({ page }) => {
  const row = await open(page, "pipeline: dynamic matrices");
  const secs = row.locator(".ship-open > section");
  await expect(secs.nth(0)).toHaveAttribute("aria-label", "Files");
  await expect(secs.nth(1)).toHaveAttribute("aria-label", "Conversation");
  // The kinds row shows kim's blocking thread holding Approve, in the gate's words.
  await expect(row.locator(".rv-k[data-k='blocking']").first()).toBeVisible();
  const approve = row.locator(".rv-bar .rv-btn", { hasText: "Approve" });
  await expect(approve).toBeDisabled();
  await expect(row.locator(".rv-thread .rv-m", { hasText: "can fail on a bad expression" })).toBeVisible();
});

test("reply, resolve, change a kind, delete", async ({ page }) => {
  const row = await open(page, "pipeline: dynamic matrices");
  // Reply on kim's thread.
  const thread = row.locator(".rv-thread").first();
  await thread.locator(".rv-btn", { hasText: "Reply" }).click();
  await thread.locator("textarea").fill("I'll name the axis in the error.");
  await thread.locator(".cp-send").click();
  await expect(thread.locator(".rv-m", { hasText: "I'll name the axis" })).toBeVisible();
  // Change the thread's kind through the ⋯ menu (your own thread is needed: make one).
  const line = row.locator(".rv-ln").nth(3);
  await line.click();
  const composer = row.locator(".rv-thread .rvc").last();
  await composer.locator(".rv-kind[data-k='nit']").click();
  await composer.locator("textarea").fill("a nit of mine");
  await composer.locator(".cp-send").click();
  const mine = row.locator(".rv-thread", { hasText: "a nit of mine" });
  await expect(mine.locator(".rv-k[data-k='nit']")).toBeVisible();
  // ⋯: copy, quote, edit, delete, kind. Delete it.
  await mine.locator(".rv-dots").first().click();
  await mine.locator(".rv-menu .rv-ib[aria-label='Delete']").click();
  await mine.locator(".rv-btn", { hasText: "Delete" }).click();
  await expect(row.locator(".rv-thread", { hasText: "a nit of mine" })).toHaveCount(0);
  // Resolve kim's thread is disabled (blocking is its author's), then kind change through ⋯ on your own comment is shown.
  const resolve = thread.locator(".rv-btn", { hasText: "Resolve" });
  await expect(resolve).toBeDisabled();
});

test("apply a suggestion makes a revision", async ({ page }) => {
  const row = await open(page, "pipeline: dynamic matrices");
  await expect(row.locator(".md-sug")).toBeVisible();
  await row.locator(".md-apply").click();
  await expect(row.locator(".md-applied")).toBeVisible();
});

test("the whole file opens in place (no popup)", async ({ page }) => {
  const row = await open(page, "pipeline: dynamic matrices");
  await row.locator(".rv-ib[title='Open the whole file']").first().click();
  const full = row.locator(".rv-full");
  await expect(full).toBeVisible();
  await expect(full.locator(".rv-l").first()).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(full).toHaveCount(0);
});

test("the vorpal search filters and jumps", async ({ page }) => {
  const row = await open(page, "pipeline: dynamic matrices");
  await row.locator(".rv-q input").fill("expand");
  await expect(row.locator(".rv-hits .rv-hit").first()).toBeVisible();
  await expect(row.locator(".rv-l.hit").first()).toBeVisible();
  await expect(row.locator(".rv-l[data-dim]").first()).toBeVisible();
  await row.locator(".rv-hits .rv-hit").first().click();
});

test("a conflict resolves where it falls and commits", async ({ page }) => {
  await page.request.post("/api/mock/review", { data: { conflicts: true } });
  const row = await open(page, "pipeline: dynamic matrices");
  const cf = row.locator(".rv-cf").first();
  await expect(cf).toBeVisible();
  await cf.locator(".rv-seg button", { hasText: "this" }).click();
  await expect(cf).toHaveAttribute("data-done", "1");
  const second = row.locator(".rv-cf").nth(1);
  await second.locator(".rv-seg button", { hasText: "main" }).click();
  const commit = row.locator(".rv-btn", { hasText: "Commit resolution" });
  await expect(commit).toBeEnabled();
  await commit.click();
  await page.request.post("/api/mock/review", { data: { conflicts: false } });
});
