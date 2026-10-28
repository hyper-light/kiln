/**
 * An open Ship item holds still and renders only what changes (docs/ci.md W26-R15, AC06), against
 * the mock's production build: 30 s of live updates (a commit arriving, review and run events
 * through /api/mock/review's emit) while an item is open and in use leaves layout-shift entries at
 * exactly 0, with or without recent input; and typing in a thread's composer renders it alone, every
 * counter outside it (the item's rows, its file diffs, its threads, the other items) at exactly 0 per
 * keystroke, read off window.__rvRenders (review/perf.ts). Live updates between keystrokes render
 * only the Files section that takes their data, nothing under it.
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
  // Every layout-shift entry with a source in Ship, with or without recent input: what moved, and by
  // how much. The one thing that may move is the composer's own text as it's typed into, right after
  // a keystroke (`typed`); anything else, typed or not, is `moved`.
  await page.addInitScript(() => {
    const w = window as unknown as { moved: string[]; typed: number };
    w.moved = [];
    w.typed = 0;
    const el = (n?: Node | null) => (n instanceof Element ? n : (n?.parentElement ?? null));
    const name = (e: Element) => `${e.tagName.toLowerCase()}${typeof e.className === "string" && e.className.trim() ? `.${e.className.trim().split(/\s+/).join(".")}` : ""}`;
    new PerformanceObserver((list) => {
      for (const e of list.getEntries() as unknown as { value: number; hadRecentInput: boolean; sources?: { node?: Node | null }[] }[]) {
        const ours = (e.sources ?? []).map((s) => el(s.node)).filter((n): n is Element => !!n?.closest(".ship"));
        if (!ours.length) continue;
        if (e.hadRecentInput && ours.every((n) => n.closest(".rvc"))) w.typed++;
        else w.moved.push(`${e.hadRecentInput ? "after input: " : ""}${ours.map(name).join(", ")} (${e.value.toFixed(5)})`);
      }
    }).observe({ type: "layout-shift", buffered: true });
  });
  try {
    const row = await open(page, "pipeline: dynamic matrices");
    // The item is in use: the pointer rests on its row and its composer holds focus.
    const box = await row.locator(".rv-s").first().boundingBox();
    if (box) await page.mouse.move(box.x + box.width / 2, box.y + 6);
    await row.locator(".rv-thread").first().locator(".rv-btn", { hasText: "Reply" }).click();
    const field = row.locator(".rv-thread textarea");
    await field.focus();
    await page.waitForTimeout(400);
    await page.evaluate(() => {
      const w = window as unknown as { moved: string[]; typed: number };
      w.moved = [];
      w.typed = 0;
    });
    // An update every ~2 s (30 a minute), each landing over 500 ms after the last keystroke, so a
    // shift it causes can't pass for the typing's; the composer must stay to be typed into.
    const end = Date.now() + 30_000;
    let updates = 0;
    while (Date.now() < end) {
      await field.pressSequentially("ab", { delay: 60 });
      await page.waitForTimeout(600);
      await live(page);
      updates++;
      await page.waitForTimeout(1300);
    }
    expect(updates).toBeGreaterThanOrEqual(13);
    expect(await page.evaluate(() => (window as unknown as { moved: string[] }).moved), "what moved").toEqual([]);
  } finally {
    await page.request.post("/api/mock/ship/arrive?clear=1");
  }
});

test("typing renders the composer alone, and live updates between keystrokes render nothing under the Files section", async ({ page }) => {
  test.setTimeout(90_000);
  await page.request.post("/api/mock/ship/arrive?clear=1");
  try {
    const row = await open(page, "pipeline: dynamic matrices");
    await row.locator(".rv-thread").first().locator(".rv-btn", { hasText: "Reply" }).click();
    const field = row.locator(".rv-thread textarea");
    await field.focus();
    const renders = () => page.evaluate(() => Object.fromEntries((window as unknown as { __rvRenders: Map<string, number> }).__rvRenders));
    const since = async (before: Record<string, number>) => Object.fromEntries(Object.entries(await renders()).map(([k, v]) => [k, v - (before[k] ?? 0)]));
    // Settled first: what opening the item and its thread set going (loads, their refreshes) is
    // done once no part has rendered for a second, so what follows is the keystrokes' alone.
    let last = JSON.stringify(await renders());
    let quiet = 0;
    for (let tries = 0; quiet < 4 && tries < 60; tries++) {
      await page.waitForTimeout(250);
      const now = JSON.stringify(await renders());
      quiet = now === last ? quiet + 1 : 0;
      last = now;
    }
    expect(quiet, "the open item never stopped rendering").toBe(4);
    // Keystrokes alone: each renders the composer it lands in, and nothing else (W26-AC06). Measured
    // over 12 keystrokes during which the page fetched nothing: data a poll (or another test's live
    // update, on the shared mock) brings in renders the Files section, and that's the next measure's.
    const fetched: string[] = [];
    page.on("request", (r) => {
      const u = new URL(r.url());
      if (u.pathname.startsWith("/api/")) fetched.push(`${r.method()} ${u.pathname}${u.search}`);
    });
    let before: Record<string, number> = {};
    let delta: Record<string, number> = {};
    let during: string[] = [];
    for (let attempt = 0; attempt < 8 && (attempt === 0 || during.length); attempt++) {
      before = await renders();
      const was = fetched.length;
      for (let i = 0; i < 12; i++) await field.pressSequentially("x", { delay: 40 });
      await page.waitForTimeout(500);
      delta = await since(before);
      during = fetched.slice(was);
    }
    expect(during, "data arrived during every run of 12 keystrokes (the last run's requests)").toEqual([]);
    expect(delta["ship/composer"] ?? 0).toBeGreaterThan(0);
    for (const k of Object.keys(delta)) if (k !== "ship/composer") expect(delta[k], `${k} rendered ${delta[k]}× on 12 keystrokes`).toBe(0);
    // Live updates between keystrokes: the Files section takes the data they refetch, and nothing
    // under it (file diffs, threads) renders again, nor the item's row or any other item.
    before = await renders();
    for (let i = 0; i < 12; i++) {
      await field.pressSequentially("x", { delay: 40 });
      if (i % 4 === 1) await live(page, false);
    }
    await page.waitForTimeout(500);
    delta = await since(before);
    for (const k of Object.keys(delta)) if (k !== "ship/composer" && k !== "ship/kilnfiles") expect(delta[k], `${k} rendered ${delta[k]}× with live updates arriving`).toBe(0);
  } finally {
    await page.request.post("/api/mock/ship/arrive?clear=1");
  }
});
