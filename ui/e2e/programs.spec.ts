/**
 * The Programs view (docs/programs.md; CP-M3), in the production build: the critical path's ten
 * tracks and 120 goals (the mock's), a first load's time, a row per track on a phone and a
 * desktop with nothing scrolling sideways, a paused track and the usage rule said, a worker
 * assigned in the view, and the person's settings kept and the coordinator told.
 */
import { expect, test, type Page } from "@playwright/test";

/** The program timed: the mock's critical path, or a real server's (`E2E_PROGRAM`). */
const CP = process.env.E2E_PROGRAM ?? "01a157fddb8276a2996246ea931c4808";
const DOCS = "plan-docs-master";
/** Against a real kiln server (playwright.config.ts): its admin token, to sign in with. */
const REAL = !!process.env.E2E_BASE_URL;
const TOKEN = process.env.E2E_TOKEN;

declare global {
  interface Window {
    __programsShown?: number;
  }
}

const rows = (page: Page, program = CP) => page.locator(`[data-program="${program}"] [data-track-row]`);

/** CP-M3: the first load with 10 tracks and 120 goals, at most 300 ms at the p95 of 20 loads. Each
 * load in a context of its own (nothing cached), timed from its navigation's start to the moment the
 * tenth track's row is in the page. One load before them, not counted, starts the browser's own
 * processes (its first page pays for them, as no visit to kiln does). */
test("a first load of the critical path's 10 tracks and 120 goals takes at most 300 ms at p95 of 20", async ({ browser }, info) => {
  test.skip(info.project.name !== "desktop", "measured once, on the desktop");
  const times: number[] = [];
  for (let i = -1; i < 20; i++) {
    const ctx = await browser.newContext({ viewport: { width: 1440, height: 900 } });
    // Signed in first on a real server (the context's cookie), outside what's timed.
    if (TOKEN) expect((await ctx.request.post("/api/auth/login", { data: { token: TOKEN } })).ok()).toBe(true);
    await ctx.addInitScript((cp) => {
      const seen = () => {
        if (window.__programsShown === undefined && document.querySelectorAll(`[data-program="${cp}"] [data-track-row]`).length >= 10) window.__programsShown = performance.now();
      };
      new MutationObserver(seen).observe(document, { childList: true, subtree: true });
    }, CP);
    const page = await ctx.newPage();
    await page.goto("/agents/programs");
    await page.waitForFunction(() => window.__programsShown !== undefined);
    const shown = await page.evaluate(() => window.__programsShown ?? Number.POSITIVE_INFINITY);
    if (i >= 0) times.push(shown);
    if (i === 0) {
      await expect(rows(page)).toHaveCount(10);
      await expect(page.locator(`[data-program="${CP}"]`)).toContainText("of 120 goals met");
    }
    await ctx.close();
  }
  const sorted = [...times].sort((a, b) => a - b);
  const p95 = sorted[Math.ceil(0.95 * sorted.length) - 1] ?? Number.POSITIVE_INFINITY;
  console.log(`first loads (ms, sorted): ${sorted.map((t) => Math.round(t)).join(" ")}; p95 ${Math.round(p95)}`);
  info.annotations.push({ type: "p95 ms", description: String(Math.round(p95)) });
  expect(p95).toBeLessThanOrEqual(300);
});

/** CP-M3: on a phone and a desktop, one row per track and nothing wider than the page. */
test("one row per track, and nothing scrolls sideways", async ({ page }, info) => {
  test.skip(REAL, "the mock's data");
  await page.goto("/agents/programs");
  await expect(rows(page)).toHaveCount(10);
  await expect(rows(page, DOCS)).toHaveCount(3);
  const ids = await rows(page).evaluateAll((els) => els.map((e) => e.getAttribute("data-track-row")));
  expect(ids).toEqual(["T1", "T2", "T3", "T4", "T5", "T6", "T7", "T8", "T9", "T10"]);
  const page_ = await page.evaluate(() => ({ scroll: document.documentElement.scrollWidth, client: document.documentElement.clientWidth }));
  expect(page_.scroll, "the page scrolls sideways").toBeLessThanOrEqual(page_.client);
  const over = await page.locator("[data-track-row], [data-track-row] *").evaluateAll((els) => els.filter((e) => e.getBoundingClientRect().right > document.documentElement.clientWidth + 0.5).length);
  expect(over, "something in a row is wider than the page").toBe(0);
  // Nor wider than its own part of the row (the choice of worker as wide as its longest name, say),
  // except the goal's part, which clips its one line.
  const spill = await rows(page).evaluateAll((els) =>
    els.flatMap((row) =>
      [...row.children]
        .filter((cell) => getComputedStyle(cell).overflowX !== "hidden")
        .flatMap((cell) => {
          const edge = cell.getBoundingClientRect().right + 0.5;
          return [...cell.querySelectorAll("*")].filter((e) => e.getBoundingClientRect().right > edge).map((e) => `${row.getAttribute("data-track-row")}: ${e.tagName.toLowerCase()}.${e.className}`);
        }),
    ),
  );
  expect(spill, "something spills out of its part of a row").toEqual([]);
  await page.screenshot({ path: info.outputPath(`programs-${info.project.name}.png`), fullPage: true });
});

/** CP-M3: the person changes the slots and a weight in the view: kept in kiln, said so. */
test("the person's slots and weights are kept", async ({ page }, info) => {
  test.skip(REAL, "the mock's data");
  test.skip(info.project.name !== "desktop", "once");
  await page.goto("/agents/programs");
  const program = page.locator(`[data-program="${CP}"]`);
  await expect(program.getByRole("button", { name: "Save settings" })).toBeDisabled();
  await program.getByRole("spinbutton", { name: "Slots", exact: true }).fill("5");
  await program.getByLabel(/^T5's weight/).fill("4");
  await program.getByRole("button", { name: "Save settings" }).click();
  await expect(program).toContainText("set by admin");
  await expect(program).toContainText("The coordinator was told now: kiln queued the message for its session");
  await expect(program.getByRole("spinbutton", { name: "Slots", exact: true })).toHaveValue("5");
  await expect(program.getByLabel(/^T5's weight/)).toHaveValue("4");
  // A weight out of bounds is refused before it's sent.
  await program.getByLabel(/^T5's weight/).fill("0");
  await expect(program).toContainText("T5's weight is a whole number from 1 to 100");
  await expect(program.getByRole("button", { name: "Save settings" })).toBeDisabled();
  // As they were, for the tests after it.
  const back = await page.request.put(`/api/programs/${CP}/settings`, { data: { slots: 6, weights: { T5: 2 } } });
  expect(back.ok()).toBe(true);
});

/** CP-M3: a track whose worker is paused at a limit says so, with when it starts over; the
 * program's usage rule shows (87%: no new goal starts), the track next for the free slot is held,
 * a goal handed to a worker is under way there, and each worker's threshold shows. */
test("a paused track, the usage rule and the thresholds say so", async ({ page }) => {
  test.skip(REAL, "the mock's data");
  await page.goto("/agents/programs");
  const program = page.locator(`[data-program="${CP}"]`);
  const row = (t: string) => program.locator(`[data-track-row="${t}"]`);
  await expect(program).toContainText("87% of a weekly window: above 80%, no new goal starts");
  await expect(row("T7")).toContainText("paused for usage");
  await expect(row("T7")).toContainText(/paused at its session limit, starts over in \dh/);
  await expect(row("T8")).toContainText("held for usage");
  await expect(row("T1")).toContainText("Under way on Amphora");
  await expect(row("T10")).toContainText("pauses at 80%: the lowest-weight track at work");
  await expect(row("T6")).toContainText("pauses at 90% (kiln's rule: 95%)");
  await expect(row("T2")).toContainText("no worker yet");
});

/** CP-M3: the person takes a track's worker off in the view: kept in kiln, and the row says so. */
test("a track's worker is assigned in the view", async ({ page }, info) => {
  test.skip(REAL, "the mock's data");
  test.skip(info.project.name !== "desktop", "once");
  await page.goto("/agents/programs");
  const program = page.locator(`[data-program="${CP}"]`);
  const t4 = program.locator('[data-track-row="T4"]');
  await expect(t4).toContainText("Isolation T4");
  await program.getByLabel("T4's worker").selectOption("");
  await program.getByRole("button", { name: "Save settings" }).click();
  await expect(t4).toContainText("no worker yet");
  const kept = await page.request.get(`/api/programs/${CP}`);
  const t = ((await kept.json()) as { tracks: { id: string; worker: unknown }[] }).tracks.find((x) => x.id === "T4");
  expect(t?.worker).toBeNull();
  // As it was, for the tests after it.
  const back = await page.request.put(`/api/programs/${CP}/tracks/T4`, { data: { agent: `${CP.slice(0, 12)}-t4` } });
  expect(back.ok()).toBe(true);
});
