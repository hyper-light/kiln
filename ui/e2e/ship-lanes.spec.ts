/**
 * The opened item's sections in their order (W26-R06, R14): Files (with the Conversation), the
 * terminal output, Jobs right above Lanes — for every item at every stage: a commit with no
 * change, a diff, a change request and a landed change. Chromium and WebKit, desktop and phone.
 */
import { expect, test, type Page } from "@playwright/test";

const REAL = !!process.env.E2E_BASE_URL;
test.skip(REAL, "drives the mock's commits");

const SECTIONS = [/^Files/, "Conversation", "Output", "Jobs", "Lanes"];

const open = async (page: Page, name: string) => {
  await page.goto("/ship");
  const row = page.locator(".ship-row", { hasText: name }).first();
  await row.locator(".ship-main").click();
  return row;
};

test("a commit with no change opens Files, Conversation, Output, Jobs, Lanes", async ({ page }) => {
  const row = await open(page, "sync: a move carries its homes");
  await expect(row.locator(".rv-s > h3, .ship-sec > h3")).toHaveText(SECTIONS);
});

test("a diff opens Files, Conversation, Output, Jobs, Lanes", async ({ page }) => {
  const row = await open(page, "pipeline: dynamic matrices");
  await expect(row.locator(".rv-s > h3, .ship-sec > h3")).toHaveText(SECTIONS);
});

test("a change request opens Files, Conversation, Output, Jobs, Lanes", async ({ page }) => {
  const row = await open(page, "sched: fair shares");
  await expect(row.locator(".rv-s > h3, .ship-sec > h3")).toHaveText(SECTIONS);
});

test("a landed change opens Files, Conversation, Output, Jobs, Lanes", async ({ page }) => {
  const row = await open(page, "ci: a landed change");
  await expect(row.locator(".rv-s > h3, .ship-sec > h3")).toHaveText(SECTIONS);
});

test("a diff's Review lane group shows each mark in order, its time and unsigned", async ({ page }) => {
  const row = await open(page, "pipeline: dynamic matrices");
  const group = row.locator('.ship-lanegroup[data-group="review"]');
  await expect(group.locator(".ship-lanegroup__head")).toContainText("Review");
  const lanes = group.locator(".ship-lane");
  // D41's story, as the step tells it: v5 pushed, kim's blocking comment, kim resolves it, v6 by
  // Apply, v7 its conflict resolved, kim's approval.
  const names = ["v5 · pushed", "kim · comment", "kim · resolved", "v6 · suggestion applied", "v7 · conflict resolved", "approval · kim (v7)"];
  await expect(lanes).toHaveCount(names.length);
  for (const [i, name] of names.entries()) {
    const lane = lanes.nth(i);
    await expect(lane).toContainText(name);
    await expect(lane).toContainText("unsigned");
    await expect(lane.locator(".ship-lane__when")).toHaveText(/\d{2}:\d{2}/);
  }
});

test("a commit with no change's Review lane group has the one mark, its push", async ({ page }) => {
  const row = await open(page, "sync: a move carries its homes");
  const lanes = row.locator('.ship-lanegroup[data-group="review"] .ship-lane');
  await expect(lanes).toHaveCount(1);
  await expect(lanes.first()).toContainText("pushed");
  await expect(lanes.first()).toContainText("unsigned");
});
