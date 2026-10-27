/**
 * An open Ship item holds still and renders only what changes (docs/ci.md W26-R15, AC06), against
 * the mock's production build: 30 s of live updates (a commit arriving, review and run events
 * through /api/mock/review's emit) while an item is open and in use leaves layout-shift entries at
 * exactly 0, and typing in a thread's composer renders it alone — every counter outside it (the
 * item's rows, its file diffs, its threads, the other items) at exactly 0 per keystroke, read off
 * window.__rvRenders (review/perf.ts).
 */
import { expect, test, type Page } from "@playwright/test";

const REAL = !!process.env.E2E_BASE_URL;
test.skip(REAL, "drives the mock's commits and events");

const open = async (page: Page, name: string) => {
  await page.goto("/ship");
  const row = page.locator(".ship-row", { hasText: name }).first();
  await row.locator(".ship-main").click();
  await expect(row.locator(".rv-s").first()).toBeVisible();
  return row;
};

/** A live update: a new commit arrives, and an event the page follows says a diff changed. */
const live = (page: Page, arrive = true) =>
  Promise.all([
    arrive ? page.request.post("/api/mock/ship/arrive") : Promise.resolve(),
    page.request.post("/api/mock/review", { data: { emit: { type: "comment", diff: "D41" } } }),
    page.request.post("/api/mock/review", { data: { emit: { type: "review", diff: "D41" } } }),
  ]);

test("an open item in use moves nothing through 30 s of live updates", async ({ page, browserName }) => {
  test.skip(browserName !== "chromium", "layout-shift entries are Chromium's");
  test.setTimeout(90_000);
  await page.request.post("/api/mock/ship/arrive?clear=1");
  await page.addInitScript(() => {
    const w = window as unknown as { shifts: number };
    w.shifts = 0;
    new PerformanceObserver((list) => {
      for (const e of list.getEntries() as unknown as { value: number; hadRecentInput: boolean; sources?: { node?: Node | null }[] }[]) {
        const el = (n?: Node | null) => (n instanceof Element ? n : (n?.parentElement ?? null));
        if (e.hadRecentInput || !(e.sources ?? []).some((s) => el(s.node)?.closest(".ship"))) continue;
        w.shifts += e.value;
      }
    }).observe({ type: "layout-shift", buffered: true });
  });
  try {
    const row = await open(page, "pipeline: dynamic matrices");
    // The item is in use: the pointer rests on its row and its composer holds focus.
    const box = await row.locator(".rv-s").first().boundingBox();
    if (box) await page.mouse.move(box.x + box.width / 2, box.y + 6);
    await row.locator(".rv-thread").first().locator(".rv-btn", { hasText: "Reply" }).click();
    await row.locator(".rv-thread textarea").focus();
    await page.waitForTimeout(400);
    await page.evaluate(() => ((window as unknown as { shifts: number }).shifts = 0));
    const end = Date.now() + 30_000;
    while (Date.now() < end) {
      await live(page);
      await page.waitForTimeout(2000);
      await row
        .locator(".rv-thread textarea")
        .pressSequentially("ab", { delay: 60 })
        .catch(() => {});
    }
    expect(await page.evaluate(() => (window as unknown as { shifts: number }).shifts)).toBe(0);
  } finally {
    await page.request.post("/api/mock/ship/arrive?clear=1");
  }
});

test("typing renders the composer alone while live updates arrive", async ({ page }) => {
  test.setTimeout(90_000);
  await page.request.post("/api/mock/ship/arrive?clear=1");
  try {
    const row = await open(page, "pipeline: dynamic matrices");
    await row.locator(".rv-thread").first().locator(".rv-btn", { hasText: "Reply" }).click();
    const field = row.locator(".rv-thread textarea");
    await field.focus();
    const renders = () => page.evaluate(() => Object.fromEntries((window as unknown as { __rvRenders: Map<string, number> }).__rvRenders));
    const before = await renders();
    for (let i = 0; i < 12; i++) {
      await field.pressSequentially("x", { delay: 40 });
      if (i % 4 === 1) await live(page, false);
    }
    await page.waitForTimeout(500);
    const after = await renders();
    const delta = Object.fromEntries(Object.entries(after).map(([k, v]) => [k, v - (before[k] ?? 0)]));
    // What a keystroke may render: the composer it lands in, and nothing else (W26-AC06).
    expect(delta["ship/composer"] ?? 0).toBeGreaterThan(0);
    for (const k of Object.keys(delta)) if (k !== "ship/composer" && k !== "ship/kilnfiles") expect(delta[k], `${k} rendered ${delta[k]}×`).toBe(0);
  } finally {
    await page.request.post("/api/mock/ship/arrive?clear=1");
  }
});
