/**
 * Ship (docs/ci.md W26-R01, R02), in the production build against the mock's commits: D41 sits
 * in Ship's list as Needs you · kim: empty matrix meant? · 1 thread, first, with Ship's Hand off
 * and Review pair; the change request's other commit folds under its head; the kind and host facets
 * narrow the list; an item opens to Files, Output, Jobs and Lanes, in that order. The list bar is the
 * filter and the sort, nothing between them, at every width.
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

  // Changes only, by their kind (no toggle of their own). The host facet: GitHub's.
  await page.goto("/ship?kind=diff,request");
  await expect(list.locator(":scope > li").first()).toBeVisible();
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
    await page.goto(`/ship?host=${host}&kind=diff,request`);
    await expect(rows.filter({ hasText: title })).toHaveCount(1);
    await expect(rows.filter({ hasText: title }).getByRole("img", { name: new RegExp(`^${review}, Land: `) })).toBeVisible();
  }
  await expect(rows.filter({ hasText: "ci: cache keys by lockfile" })).toHaveCount(0);
  await page.goto("/ship?host=bitbucket");
  await expect(rows.first()).toContainText("Landing");
  // The landed change is GitLab's !6, not a kiln diff: kiln's diff is D41, its change request CR12.
  await page.goto("/ship?kind=diff");
  await expect(rows).toHaveCount(1);
  await expect(rows.first()).toContainText("D41");
  await page.goto("/ship?kind=request,diff&host=kiln");
  await expect(rows).toHaveCount(2);
});

test("the list bar is the filter and the sort, nothing between them, from 320 to 1440 px", async ({ page, isMobile }) => {
  test.skip(isMobile, "the widths are set here; the desktop projects run them");
  test.setTimeout(90_000);
  for (const width of [320, 390, 430, 640, 641, 800, 1280, 1440]) {
    await page.setViewportSize({ width, height: 900 });
    await page.goto("/ship");
    const list = page.getByRole("list", { name: "Commits", exact: true });
    await expect(list.locator(":scope > li").first()).toBeVisible({ timeout: 15_000 });
    const tools = page.locator(".ship-tools");
    // The filter and the sort: no toggle beside them (Ada, 2026-10-28).
    await expect(tools.locator(":scope > *")).toHaveCount(2);
    await expect(tools.getByRole("group")).toHaveCount(0);
    const box = async (sel: string) => (await page.locator(sel).first().boundingBox()) ?? { x: 0, y: 0, width: 0, height: 0 };
    const [bar, filter, field, sort, rows] = [
      await box(".ship-tools"),
      await box(".ship-tools .ship-facet"),
      await box(".ship-tools .ship-facet .field"),
      await box(".ship-tools .ship-sort"),
      await box(".ship-list"),
    ];
    const at = `at ${width}: filter ${JSON.stringify(filter)}, its field ${JSON.stringify(field)}, sort ${JSON.stringify(sort)}`;
    // The filter is its field's height: no room held under it.
    expect(Math.abs(filter.height - field.height), at).toBeLessThanOrEqual(1);
    if (width <= 640) {
      // On a phone the filter takes the width and the sort sits right under it.
      expect(Math.abs(sort.y - (filter.y + filter.height) - 8), at).toBeLessThanOrEqual(1);
      expect(Math.abs(filter.width - bar.width), at).toBeLessThanOrEqual(1);
      expect(Math.abs(sort.width - bar.width), at).toBeLessThanOrEqual(1);
    } else {
      // Wider, they share a line: the filter on the left, the sort on the right.
      expect(Math.abs(sort.y - filter.y), at).toBeLessThanOrEqual(1);
      expect(sort.x, at).toBeGreaterThan(filter.x + filter.width);
    }
    // And the list comes right after the bar.
    expect(rows.y - (bar.y + bar.height), at).toBeLessThanOrEqual(13);
  }
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
