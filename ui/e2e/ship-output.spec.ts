/**
 * The opened commit's Output (plan 01a17ba155a17737a39348c6c5362210, step 3), against the mock: the
 * first row's picked job (Checks, streaming one of the mock's own jobs) shows in a card exactly the
 * lanes card's width from 320 to 1440 (the Opened commit plan's Output step) at 1440, 1280, 1000, 800,
 * 600, 390 and 320; as the job prints, nothing moves; scrolled up, the log stays where it was.
 */
import { expect, test, type Locator } from "@playwright/test";

const REAL = !!process.env.E2E_BASE_URL;
test.skip(REAL, "drives the mock's commits");

/** The Output card's width, the lanes card's inner width, and where the card sits. */
const measure = (row: Locator) =>
  row.evaluate((li) => {
    const out = li.querySelector(".ship-output");
    const lanes = li.querySelector('section[aria-label="Lanes"]');
    if (!out || !lanes) return null;
    const cs = getComputedStyle(lanes);
    const inner = lanes.getBoundingClientRect().width - parseFloat(cs.paddingLeft) - parseFloat(cs.paddingRight) - parseFloat(cs.borderLeftWidth) - parseFloat(cs.borderRightWidth);
    const r = out.getBoundingClientRect();
    return { width: r.width, lanes: inner, top: Math.round(r.top), height: Math.round(r.height) };
  });

for (const width of [1440, 1280, 1000, 800, 600, 390, 320]) {
  test(`at ${width}, the output is the lanes card's width and holds still as the job prints`, async ({ page, isMobile, browserName }) => {
    test.skip(isMobile, "each width is set here; the desktop projects run them all");
    await page.setViewportSize({ width, height: 900 });
    await page.goto("/ship");
    const row = page.getByRole("list", { name: "Commits", exact: true }).locator(":scope > li").first();
    await row.getByRole("button", { name: /^Open / }).click();
    await expect(row.locator(".ship-output .xterm")).toBeVisible({ timeout: 15_000 });
    // Review's Files above it drawn first: what's measured is lines arriving, not Files finishing its load.
    await expect(row.locator('section[aria-label="Files"]')).toBeVisible({ timeout: 15_000 });
    const first = await measure(row);
    expect(first).not.toBeNull();
    expect(Math.abs((first?.width ?? 0) - (first?.lanes ?? 0))).toBeLessThanOrEqual(1);

    // What's measured is lines arriving, so the fonts are in first (a late swap isn't a line).
    await page.evaluate(() => document.fonts.ready.then(() => undefined));
    if (browserName === "chromium")
      await page.evaluate(() => {
        const w = window as unknown as { __shift: number; __moved: string[] };
        w.__shift = 0;
        w.__moved = [];
        new PerformanceObserver((l) => {
          for (const e of l.getEntries() as unknown as { value: number; hadRecentInput: boolean; sources?: { node?: Node; previousRect: DOMRectReadOnly; currentRect: DOMRectReadOnly }[] }[]) {
            if (e.hadRecentInput) continue;
            w.__shift += e.value;
            for (const s of e.sources ?? []) {
              const n = s.node as HTMLElement | undefined;
              w.__moved.push(
                `${n?.nodeName ?? "?"}.${typeof n?.className === "string" ? n.className.slice(0, 50) : ""} "${(n?.textContent ?? "").slice(0, 30)}" x${Math.round(s.previousRect.x)}->${Math.round(s.currentRect.x)} y${Math.round(s.previousRect.y)}->${Math.round(s.currentRect.y)}`,
              );
            }
          }
        }).observe({ type: "layout-shift" });
      });
    await page.waitForTimeout(3000);
    const after = await measure(row);
    expect(after?.top).toBe(first?.top);
    expect(after?.height).toBe(first?.height);
    if (browserName === "chromium") {
      const seen = await page
        .evaluate(() => window as unknown as { __shift: number; __moved: string[] })
        .then(() => page.evaluate(() => ({ shift: (window as unknown as { __shift: number }).__shift, moved: (window as unknown as { __moved: string[] }).__moved })));
      expect(seen.shift, `what moved: ${seen.moved.join(" | ")}`).toBe(0);
    }
  });
}

test("scrolled up, the output stays where you are", async ({ page, isMobile }) => {
  test.skip(isMobile, "the desktop projects run it");
  await page.goto("/ship");
  const row = page.getByRole("list", { name: "Commits", exact: true }).locator(":scope > li").first();
  await row.getByRole("button", { name: /^Open / }).click();
  const viewport = row.locator(".ship-output .xterm-viewport");
  await expect(viewport).toBeAttached({ timeout: 15_000 });
  await viewport.evaluate((v) => v.scrollTo({ top: 0 }));
  await viewport.dispatchEvent("wheel", { deltaY: -400 });
  await page.waitForTimeout(2500);
  expect(await viewport.evaluate((v) => v.scrollTop)).toBe(0);
});

/** Opens the first row at a job of its Checks that has run and waits for the log; its job and step. */
async function openOutput(page: import("@playwright/test").Page) {
  await page.goto("/ship");
  const row = page.getByRole("list", { name: "Commits", exact: true }).locator(":scope > li").first();
  await row.getByRole("button", { name: /^Open / }).click();
  const checks = row.getByRole("list", { name: "Checks jobs" });
  await expect(checks.getByRole("button").first()).toBeVisible({ timeout: 15_000 });
  await checks.getByRole("button").last().click();
  const out = row.locator(".ship-output");
  await expect(out.locator(".xterm")).toBeVisible({ timeout: 15_000 });
  return { out, url: `/api/jobs/${encodeURIComponent((await out.getAttribute("data-job")) ?? "")}/logs?step=${(await out.getAttribute("data-step")) ?? "0"}` };
}

test("copy puts the job step's whole output on the clipboard", async ({ page, isMobile }) => {
  test.skip(isMobile, "the desktop projects run it");
  await page.addInitScript(() => {
    const w = window as unknown as { __copied?: string };
    Object.defineProperty(navigator, "clipboard", { configurable: true, value: { writeText: async (t: string) => void (w.__copied = t) } });
  });
  const { out } = await openOutput(page);
  // A running job moves from step to step, so each copy is checked against the very log it fetched
  // (GET /api/jobs/:id/logs?step=N for the job and step the card showed), until one carries text.
  let copied = "";
  let url = "";
  for (let i = 0; i < 15 && !copied; i++) {
    const got = page.waitForResponse((r) => /\/api\/jobs\/[^/]+\/logs\?step=\d+$/.test(r.url()) && r.request().method() === "GET");
    await out.getByRole("button", { name: /^Cop(y|ied) the output$/ }).click();
    const res = await got;
    const fetched = await res.text();
    await expect.poll(() => page.evaluate(() => (window as unknown as { __copied?: string }).__copied ?? null)).toBe(fetched);
    copied = fetched;
    url = new URL(res.url()).pathname + new URL(res.url()).search;
    if (!copied) await page.waitForTimeout(1000);
  }
  expect(copied.length).toBeGreaterThan(0);
  expect(url).toMatch(new RegExp(`^/api/jobs/${encodeURIComponent((await out.getAttribute("data-job")) ?? "")}/logs\\?step=\\d+$`));
  // The log only grows: asked again, it starts with what was copied.
  expect((await (await page.request.get(url)).text()).startsWith(copied)).toBe(true);
  await expect(out.getByRole("button", { name: "Copied the output" })).toBeVisible();
});

test("a refused copy says so", async ({ page, isMobile }) => {
  test.skip(isMobile, "the desktop projects run it");
  await page.addInitScript(() => {
    Object.defineProperty(navigator, "clipboard", { configurable: true, value: { writeText: async () => Promise.reject(new DOMException("Write permission denied.", "NotAllowedError")) } });
  });
  const { out } = await openOutput(page);
  await out.getByRole("button", { name: "Copy the output" }).click();
  await expect(page.getByText("Couldn't copy the output")).toBeVisible();
});
