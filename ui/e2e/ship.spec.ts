/**
 * Ship (docs/ci.md W26-R01, R02), in the production build against the mock's commits: D41 sits
 * in Ship's list as Needs you · kim: empty matrix meant? · 1 thread, first, with Ship's Hand off
 * and Review pair; the change request's other commit folds under its head; the Review lens and the
 * host facet narrow the list; an item opens to Files, Output, Jobs and Lanes, in that order.
 */
import { expect, test } from "@playwright/test";

const REAL = !!process.env.E2E_BASE_URL;
test.skip(REAL, "drives the mock's commits");

test("changes are Ship items, drawn by Ship's own row, needs you first", async ({ page }) => {
  await page.goto("/ship");
  const list = page.getByRole("list", { name: "Commits", exact: true });
  const rows = list.locator(":scope > li");
  await expect(rows.first()).toContainText("pipeline: dynamic matrices");
  await expect(rows.first()).toContainText("Needs you");
  await expect(rows.first()).toContainText("kim: empty matrix meant? · 1 thread");
  await expect(rows.first()).toContainText("D41");
  await expect(rows.first().getByRole("img", { name: /^Review: requested, Land: not yet, Checks: passed/ })).toBeVisible();
  await expect(rows.first().getByRole("button", { name: "Hand off" })).toBeVisible();
  await expect(rows.first().getByRole("button", { name: "Review" })).toBeVisible();

  // D41's first revision, superseded by its second: quiet, folded under it.
  await rows
    .first()
    .getByRole("button", { name: /Show 1 more commit of D41/ })
    .click();
  await expect(rows.first().getByRole("list", { name: "The other commits of D41" })).toContainText("Superseded");

  // The change request's other commit, folded under its head.
  const cr = rows.filter({ hasText: "sched: fair shares" });
  await cr.getByRole("button", { name: /Show 1 more commit of CR12/ }).click();
  await expect(cr.getByRole("list", { name: "The other commits of CR12" })).toContainText("sched: weights");

  // Review opens the item at its files; the dropdown's order.
  await rows.first().getByRole("button", { name: "Review" }).click();
  const sections = rows.first().locator(".rv-s > h3, .ship-sec > h3");
  await expect(sections).toHaveText([/^Files/, "Conversation", "Output", "Jobs", "Lanes"]);

  // The Review lens: changes only. The host facet: GitHub's.
  await page.getByRole("group", { name: "Lens" }).getByRole("button", { name: "Review" }).click();
  await expect(page).toHaveURL(/lens=review/);
  await expect(list).not.toContainText("deploy: a rollback holds");
  await page.goto("/ship?host=github");
  await expect(page.getByRole("list", { name: "Commits", exact: true }).locator(":scope > li")).toHaveCount(1);
  await expect(page.getByRole("list", { name: "Commits", exact: true })).toContainText("Faster builds");
});

test("each host's pull requests and kiln's diffs are Ship items, Review first on their strip; host and kind narrow the list", async ({ page }) => {
  const rows = page.getByRole("list", { name: "Commits", exact: true }).locator(":scope > li");
  for (const [host, title, review] of [
    ["github", "Faster builds", "Review: requested"],
    ["gitlab", "docs: the runner's limits", "Review: changes asked"],
    ["bitbucket", "ci: cache keys by lockfile", "Review: approved"],
    ["kiln", "pipeline: dynamic matrices", "Review: requested"],
  ] as const) {
    await page.goto(`/ship?host=${host}&lens=review`);
    await expect(rows.filter({ hasText: title })).toHaveCount(1);
    await expect(rows.filter({ hasText: title }).getByRole("img", { name: new RegExp(`^${review}, Land: `) })).toBeVisible();
  }
  await expect(rows.filter({ hasText: "ci: cache keys by lockfile" })).toHaveCount(0);
  await page.goto("/ship?host=bitbucket");
  await expect(rows.first()).toContainText("Landing");
  await page.goto("/ship?kind=diff");
  await expect(rows).toHaveCount(1);
  await expect(rows.first()).toContainText("D41");
  await page.goto("/ship?kind=request,diff&host=kiln");
  await expect(rows).toHaveCount(2);
});

test("Ship's list holds still as it loads and as a row opens", async ({ page, browserName }) => {
  test.skip(browserName !== "chromium", "layout-shift entries are Chromium's");
  await page.addInitScript(() => {
    type Rect = { x: number; y: number; width: number; height: number };
    type Shift = { value: number; hadRecentInput: boolean; sources?: { node?: Node | null; previousRect: Rect; currentRect: Rect }[] };
    const w = window as unknown as { shifts: string[] };
    w.shifts = [];
    const r = (b: Rect) => `${Math.round(b.x)},${Math.round(b.y)} ${Math.round(b.width)}x${Math.round(b.height)}`;
    // Ship's own page: what the app's frame around it does is the frame's.
    const el = (n?: Node | null) => (n instanceof Element ? n : (n?.parentElement ?? null));
    new PerformanceObserver((list) => {
      for (const e of list.getEntries() as unknown as Shift[]) {
        const mine = (e.sources ?? []).filter((s) => el(s.node)?.closest(".ship"));
        if (e.hadRecentInput || mine.length === 0) continue;
        w.shifts.push(`${e.value.toFixed(6)}: ${mine.map((s) => `${s.node?.nodeName ?? "?"}.${el(s.node)?.className ?? ""} ${r(s.previousRect)} -> ${r(s.currentRect)}`).join("; ")}`);
      }
    }).observe({ type: "layout-shift", buffered: true });
  });
  await page.goto("/ship");
  const rows = page.getByRole("list", { name: "Commits", exact: true }).locator(":scope > li");
  await expect(rows.first()).toContainText("pipeline: dynamic matrices");
  await page.waitForTimeout(600);
  const shifts = () => page.evaluate(() => (window as unknown as { shifts: string[] }).shifts);
  expect(await shifts()).toEqual([]);
  await rows.first().getByRole("button", { name: "pipeline: dynamic matrices" }).click();
  await expect(rows.first().locator(".rv-s > h3, .ship-sec > h3")).toHaveText([/^Files/, "Conversation", "Output", "Jobs", "Lanes"]);
  await page.waitForTimeout(600);
  expect(await shifts()).toEqual([]);
});
