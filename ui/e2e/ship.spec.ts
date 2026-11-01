/**
 * Ship (docs/ci.md W26-R01, R02, R29), in the production build against the mock's commits: kiln's
 * row (Ship's rows are repositories) shows its newest, D41, as Needs you · kim: empty matrix meant? ·
 * 1 thread, first, with Ship's Hand off and Review pair; opened, its commits hold D41's first revision
 * and the change request's other commit; the kind and host facets narrow the list; an item opens to
 * its Commits, then Files, Jobs (with the picked job's output) and Lanes, in that order. The list bar is the
 * filter and the sort, nothing between them, at every width; the pace line under it says what's
 * moving in one line (plan 01a1856fe81375b1bd1d1d86f270a45b, steps 1 and 5).
 */
import { expect, test } from "@playwright/test";
import type { ShipPage } from "../src/api/ship";

const REAL = !!process.env.E2E_BASE_URL;
test.skip(REAL, "drives the mock's commits");

test("changes are Ship items, drawn by Ship's own row, needs you first", async ({ page }) => {
  await page.goto("/ship");
  const list = page.getByRole("list", { name: "Repositories", exact: true });
  const rows = list.locator(":scope > li");
  await expect(rows.first()).toContainText("pipeline: dynamic matrices");
  await expect(rows.first()).toContainText("Needs you");
  await expect(rows.first()).toContainText("kim: empty matrix meant? · 1 thread");
  await expect(rows.first()).toContainText("D41");
  await expect(rows.first().getByRole("img", { name: /^Review: requested, Land: not yet, Checks: passed/ })).toBeVisible();
  await expect(rows.first().getByRole("button", { name: "Hand off" })).toBeVisible();
  await expect(rows.first().getByRole("button", { name: "Review" })).toBeVisible();

  // Review opens the item at its files; the dropdown's order: the repository's commits first.
  await rows.first().getByRole("button", { name: "Review" }).click();
  const sections = rows.first().locator(".rv-s > h3, .ship-sech > h3");
  await expect(sections).toHaveText(["Commits", /^Files/, "Conversation", "Jobs", "Lanes"]);
  // Among its commits: D41's first revision, superseded by its second, and the change request's other commit.
  const commits = rows.first().locator("section.ship-commits");
  await expect(commits.locator(".sc-c", { hasText: "2222222" }).locator('svg.sg[data-state="superseded"]')).toBeVisible();
  await expect(commits.locator(".sc-c", { hasText: "sched: weights" })).toHaveCount(1);

  // Changes only, by their kind (no toggle of their own). The host facet: GitHub's.
  await page.goto("/ship?kind=diff,request");
  await expect(list.locator(":scope > li").first()).toBeVisible();
  await expect(list).not.toContainText("deploy: a rollback holds");
  await page.goto("/ship?host=github");
  await expect(page.getByRole("list", { name: "Repositories", exact: true }).locator(":scope > li")).toHaveCount(1);
  await expect(page.getByRole("list", { name: "Repositories", exact: true })).toContainText("Faster builds");
});

test("each host's pull requests and kiln's diffs are Ship items, Review first on their strip; host and kind narrow the list", async ({ page }) => {
  const rows = page.getByRole("list", { name: "Repositories", exact: true }).locator(":scope > li");
  // A host chosen: the repositories with its changes (the mock's are kiln's), each showing its newest
  // of them, Review first on its strip; opened, its commits are the host's, that newest first and
  // picked, the host's change among them.
  for (const [host, title, n] of [
    ["github", "Faster builds", 1],
    ["gitlab", "docs: the runner's limits", 2],
    ["bitbucket", "ci: cache keys by lockfile", 1],
    ["kiln", "pipeline: dynamic matrices", 4],
  ] as const) {
    await page.goto(`/ship?host=${host}`);
    await expect(rows).toHaveCount(1);
    await expect(rows.first().getByRole("img", { name: /^Review: [^,]+, Land: / })).toBeVisible();
    await rows
      .first()
      .getByRole("button", { name: /^Open / })
      .click();
    const lines = rows.first().locator("section.ship-commits .sc-c");
    await expect(lines).toHaveCount(n, { timeout: 15_000 });
    await expect(lines.first()).toHaveAttribute("data-sel", "true");
    await expect(lines.filter({ hasText: title }).first()).toBeVisible();
  }
  await page.goto("/ship?host=bitbucket");
  await expect(rows.first()).toContainText("Landing");
  // The landed change is GitLab's !6, not a kiln diff: kiln's diff is D41, its change request CR12.
  await page.goto("/ship?kind=diff");
  await expect(rows).toHaveCount(1);
  await expect(rows.first()).toContainText("D41");
  // Opened, its commits are its diffs: D41's two revisions.
  await rows
    .first()
    .getByRole("button", { name: /^Open / })
    .click();
  await expect(rows.first().locator("section.ship-commits .sc-c")).toHaveText([/4444444/, /2222222/], { timeout: 15_000 });
  await page.goto("/ship?kind=request,diff&host=kiln");
  await expect(rows).toHaveCount(1);
});

test("the list bar is the filter and the sort, nothing between them, from 320 to 1440 px", async ({ page, isMobile }) => {
  test.skip(isMobile, "the widths are set here; the desktop projects run them");
  test.setTimeout(90_000);
  for (const width of [320, 390, 430, 640, 641, 800, 1280, 1440]) {
    await page.setViewportSize({ width, height: 900 });
    await page.goto("/ship");
    const list = page.getByRole("list", { name: "Repositories", exact: true });
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
    // And the pace line comes right after the bar, the list right after it.
    const pace = await box(".ship-pace");
    expect(pace.y - (bar.y + bar.height), at).toBeLessThanOrEqual(13);
    expect(rows.y - (pace.y + pace.height), at).toBeLessThanOrEqual(11);
  }
});

test("the pace line says what's moving, in the server's numbers, on one line from 320 to 1440 px", async ({ page, isMobile }) => {
  test.skip(isMobile, "the widths are set here; the desktop projects run them");
  test.setTimeout(90_000);
  const pace = ((await (await page.request.get("/api/ship/commits")).json()) as ShipPage).pace;
  if (!pace) throw new Error("the list came with no pace");
  // Each environment named by its repository's name where there's more than one repository.
  const several = new Set(pace.envs.map((e) => e.repo)).size > 1;
  const words = [
    `${pace.in_flight} in flight · ${pace.agents} agent${pace.agents === 1 ? "" : "s"}`,
    ...pace.envs.map((e) => `${several ? `${e.repo.split("/").pop()} ` : ""}${e.environment} ${e.short}${e.behind === null ? "" : ` · ${e.behind} behind`}`),
    ...(pace.runners ? [`runners ${pace.runners.on}/${pace.runners.of}`] : []),
  ];
  expect(words.length).toBeGreaterThan(1);
  for (const width of [320, 390, 640, 641, 1280, 1440]) {
    await page.setViewportSize({ width, height: 900 });
    await page.goto("/ship");
    const line = page.getByLabel("Pace", { exact: true });
    await expect(line).toBeVisible({ timeout: 15_000 });
    await expect(line.locator(":scope > span")).toHaveText(words);
    // One line however narrow: it scrolls sideways rather than wrap.
    expect((await line.boundingBox())?.height ?? 0, `at ${width}`).toBeLessThanOrEqual(19);
  }
  // Commits arriving leave it one line where it is.
  await page.setViewportSize({ width: 390, height: 900 });
  await page.goto("/ship");
  const line = page.getByLabel("Pace", { exact: true });
  await expect(line).toBeVisible({ timeout: 15_000 });
  const before = await line.boundingBox();
  try {
    await page.request.post("/api/mock/ship/agents?n=6&over=2");
    await page.waitForTimeout(3_000);
    expect(await line.boundingBox()).toEqual(before);
  } finally {
    await page.request.post("/api/mock/ship/arrive?clear=1");
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
  const rows = page.getByRole("list", { name: "Repositories", exact: true }).locator(":scope > li");
  await expect(rows.first()).toContainText("pipeline: dynamic matrices");
  await page.waitForTimeout(600);
  const shifts = () => page.evaluate(() => (window as unknown as { shifts: string[] }).shifts);
  expect(await shifts()).toEqual([]);
  await rows
    .first()
    .getByRole("button", { name: /^Open / })
    .click();
  await expect(rows.first().locator(".rv-s > h3, .ship-sech > h3")).toHaveText(["Commits", /^Files/, "Conversation", "Jobs", "Lanes"]);
  await page.waitForTimeout(600);
  expect(await shifts()).toEqual([]);
});
