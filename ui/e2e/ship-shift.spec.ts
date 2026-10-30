/**
 * A row open through updates moves nothing (Agent pace and checks plan
 * 01a175b7f2837279be2a229ee94ac1f0, step 2), against the mock: with one row open, 30 s of updates
 * at agents' pace (14 agents' commits every 10 s, each starting CI as a push does, and review events
 * on D41 every 2 s) leave the page's cumulative layout shift at exactly 0, every layout-shift entry
 * counted, wherever on the page and with or without input. A change's row, a failing commit's and a
 * shipped one's, at 1280 and 390 px; the window tall enough that the whole open row is on screen.
 */
import { expect, test, type Page } from "@playwright/test";

const REAL = !!process.env.E2E_BASE_URL;
test.skip(REAL, "drives the mock's commits");

/** What the page keeps of each layout shift: its value, and what moved, from where to where. */
interface Shift {
  value: number;
  input: boolean;
  what: string[];
}

/** Every layout-shift entry of Ship's from here on (a source in `.ship`, as ship-perf.spec counts them: the
 * app's approval notices come and go on their own), kept on `window.shifts`. */
const watch = (page: Page) =>
  page.addInitScript(() => {
    const w = window as unknown as { shifts: Shift[] };
    w.shifts = [];
    const name = (n?: Node | null) => {
      const e = n instanceof Element ? n : (n?.parentElement ?? null);
      if (!e) return "?";
      const cls = typeof e.className === "string" && e.className.trim() ? `.${e.className.trim().split(/\s+/).join(".")}` : "";
      const label = e.getAttribute("aria-label") ?? e.closest("section[aria-label]")?.getAttribute("aria-label") ?? "";
      return `${e.tagName.toLowerCase()}${cls}${label ? ` in "${label}"` : ""}`;
    };
    const rect = (r: DOMRectReadOnly) => `${Math.round(r.x)},${Math.round(r.y)} ${Math.round(r.width)}x${Math.round(r.height)}`;
    new PerformanceObserver((list) => {
      for (const e of list.getEntries() as unknown as { value: number; hadRecentInput: boolean; sources?: { node?: Node | null; previousRect: DOMRectReadOnly; currentRect: DOMRectReadOnly }[] }[])
        if ((e.sources ?? []).some((s) => (s.node instanceof Element ? s.node : (s.node?.parentElement ?? null))?.closest(".ship")))
          w.shifts.push({ value: e.value, input: e.hadRecentInput, what: (e.sources ?? []).map((s) => `${name(s.node)} ${rect(s.previousRect)} → ${rect(s.currentRect)}`) });
    }).observe({ type: "layout-shift", buffered: true });
  });

/** The open row loaded and still: its sections drawn, and no shift for a second. */
const settle = async (page: Page) => {
  for (let quiet = 0, tries = 0; quiet < 4 && tries < 80; tries++) {
    const n = await page.evaluate(() => (window as unknown as { shifts: Shift[] }).shifts.length);
    await page.waitForTimeout(250);
    quiet = (await page.evaluate(() => (window as unknown as { shifts: Shift[] }).shifts.length)) === n ? quiet + 1 : 0;
  }
};

const ROWS = [
  { name: "pipeline: dynamic matrices", what: "a change under review", loaded: 'section[aria-label="Files"] .rv-sum' },
  { name: "sync: a move carries its homes", what: "a failing commit", loaded: 'section[aria-label="Lanes"] .ship-lanesbox' },
  { name: "ci: a landed change", what: "a shipped commit", loaded: 'section[aria-label="Lanes"] .ship-lanesbox' },
];

for (const width of [1280, 390])
  for (const row of ROWS)
    test(`at ${width}, ${row.what} open through 30 s of updates at agents' pace: cumulative layout shift exactly 0`, async ({ page, browserName, isMobile }) => {
      test.skip(browserName !== "chromium" || isMobile, "layout-shift entries are Chromium's; the desktop project sets the width");
      test.setTimeout(120_000);
      await page.request.post("/api/mock/ship/arrive?clear=1");
      await watch(page);
      try {
        await page.setViewportSize({ width, height: 2400 });
        await page.goto("/ship");
        const it = page.locator(".ship-list > li.ship-row").filter({ hasText: row.name }).first();
        await expect(it).toBeVisible({ timeout: 15_000 });
        await it.locator(".ship-go").click();
        await expect(it.locator(row.loaded).first()).toBeVisible({ timeout: 15_000 });
        await it.scrollIntoViewIfNeeded();
        // The pointer away from the page's controls, resting; opened, the list holds by itself.
        await page.mouse.move(width - 2, 2399);
        await settle(page);
        await page.evaluate(() => ((window as unknown as { shifts: Shift[] }).shifts = []));
        // 30 s of updates: 42 agents' commits (14 every 10 s), and review events on D41 every 2 s.
        await page.request.post("/api/mock/ship/agents?n=42&over=30");
        const end = Date.now() + 30_000;
        let events = 0;
        while (Date.now() < end) {
          // Counted when both reach the mock (a loaded machine can drop one: it's a stimulus, not a measure).
          const sent = await Promise.all(
            (["comment", "review"] as const).map((type) =>
              page.request.post("/api/mock/review", { data: { emit: { type, diff: "D41" } } }).then(
                () => true,
                () => false,
              ),
            ),
          );
          if (sent.every(Boolean)) events++;
          await page.waitForTimeout(2000);
        }
        await page.waitForTimeout(1000);
        expect(events).toBeGreaterThanOrEqual(13);
        const shifts = await page.evaluate(() => (window as unknown as { shifts: Shift[] }).shifts);
        const cls = shifts.reduce((s, e) => s + e.value, 0);
        test.info().annotations.push({ type: "cls", description: `${cls} from ${shifts.length} shifts` });
        expect(
          shifts.map((s) => `${s.value.toFixed(5)}${s.input ? " after input" : ""}: ${s.what.join("; ")}`),
          "what moved",
        ).toEqual([]);
        expect(cls).toBe(0);
        await expect(it.locator(".ship-go")).toHaveAttribute("aria-expanded", "true");
      } finally {
        await page.request.post("/api/mock/ship/arrive?clear=1");
      }
    });
