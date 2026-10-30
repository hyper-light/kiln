/**
 * An opened commit's Jobs as one pane (docs/ci.md W26-R23, as Ada approved it on 2026-10-30), against
 * the mock's failing row (3333333): the strip over the jobs by stage, the picked job's output beside
 * them on a desktop and under its line on a phone, where the strip comes again. A line picks its job
 * alone and its glyph adds it to the pick; the words pick failed, running, waiting or all; Retry,
 * Pause (Resume) and Cancel are one width, dimmed where they don't apply, and each reaches
 * `POST /api/ship/jobs` with what's picked; Cancel asks again; a refusal says why in the strip;
 * nothing moves and nothing scrolls sideways.
 */
import { expect, test, type Locator, type Page } from "@playwright/test";

const REAL = !!process.env.E2E_BASE_URL;
test.skip(REAL, "drives the mock's commits");

/** Opens the failing row; its Jobs pane, and the commit. */
async function open(page: Page): Promise<{ pane: Locator; sha: string }> {
  await page.request.post("/api/mock/ship/jobs?reset=1");
  await page.goto("/ship");
  const row = page.locator(".ship-row").filter({ hasText: "3333333" }).first();
  await row.getByRole("button", { name: /^Open / }).click();
  const pane = row.getByRole("region", { name: "Jobs" });
  await expect(pane.locator(".jp-strip").first()).toBeVisible({ timeout: 15_000 });
  await expect(pane.locator(".jp-j[data-job]").first()).toBeVisible({ timeout: 15_000 });
  const detail = (await (await page.request.get("/api/ship/commits?repo=hyper-light/kiln")).json()) as { commits: { sha: string; short: string }[] };
  return { pane, sha: detail.commits.find((c) => c.short === "3333333")?.sha ?? "" };
}

/** The strip at the top of the pane, its words and its actions. */
const strip = (pane: Locator) => pane.locator(".jp-strip").first();
const verb = (pane: Locator, name: string | RegExp) => strip(pane).locator(".jp-verbs button", { hasText: name });
const word = (pane: Locator, w: string) => strip(pane).locator(".jp-q button", { hasText: w });

/** The bodies this browser sent `POST /api/ship/jobs`, as the mock kept them. */
const sent = async (page: Page) => ((await (await page.request.get("/api/mock/ship/jobs")).json()) as { bodies: Record<string, unknown>[] }).bodies;

test("the pane as approved: the strip over the jobs by stage, the picked job's output beside them, or under its line with the strip again on a phone", async ({ page, isMobile }) => {
  const { pane } = await open(page);
  await expect(pane.locator(".jp-g > span:first-child")).toContainText(["Review", "Land", "Checks"]);
  // Three actions of one width, never hidden.
  const widths = await strip(pane)
    .locator(".jp-verbs button")
    .evaluateAll((bs) => bs.map((b) => Math.round(b.getBoundingClientRect().width)));
  expect(widths).toHaveLength(3);
  expect(new Set(widths).size).toBe(1);
  await expect(strip(pane).locator(".jp-q button")).toHaveText(["failed", "running", "waiting", "all"]);
  // The line shown has the straight edge; its output is beside the list, or under it.
  const at = pane.locator(".jp-j[data-at]");
  await expect(at).toHaveCount(1);
  if (isMobile) {
    await expect(pane.locator(".jp-side")).toHaveCount(0);
    const under = at.locator("xpath=following-sibling::li[1]");
    await expect(under).toHaveClass(/jp-detail/);
    await expect(under.locator(".jp-strip")).toBeVisible();
    await expect(under.locator(".ship-output, .ship-output-slot").first()).toBeVisible({ timeout: 15_000 });
  } else {
    await expect(pane.locator(".jp-side .ship-output, .jp-side .ship-output-slot").first()).toBeVisible({ timeout: 15_000 });
    await expect(pane.locator(".jp-detail")).toHaveCount(0);
  }
  // Flat: no box round the strip, the list or the output.
  for (const sel of [".jp-strip", ".jp-list", ".jp .ship-output"]) {
    const r = await pane
      .locator(sel)
      .first()
      .evaluate((el) => ({ radius: getComputedStyle(el).borderTopLeftRadius, left: getComputedStyle(el).borderLeftWidth }));
    expect(r.radius, sel).toBe("0px");
    expect(r.left, sel).toBe("0px");
  }
});

test("a line picks its job alone; its glyph adds it to the pick, or takes it out", async ({ page }) => {
  const { pane } = await open(page);
  const jobs = pane.locator(".jp-j[data-job]");
  const first = jobs.nth(0);
  const second = jobs.nth(1);
  await first.locator(".jp-line").click();
  await expect(first).toHaveAttribute("data-at", "true");
  await expect(pane.locator(".jp-j[data-sel]")).toHaveCount(1);
  await second.locator(".jp-glyph").click();
  await expect(pane.locator(".jp-j[data-sel]")).toHaveCount(2);
  await expect(strip(pane).locator(".jp-said")).toContainText("2 jobs picked");
  await second.locator(".jp-glyph").click();
  await expect(pane.locator(".jp-j[data-sel]")).toHaveCount(1);
  await second.locator(".jp-line").click();
  await expect(second).toHaveAttribute("data-at", "true");
  await expect(pane.locator(".jp-j[data-sel]")).toHaveCount(1);
});

test("a pick made by hand reaches the endpoint as its jobs, those an action applies to", async ({ page }) => {
  const { pane, sha } = await open(page);
  const waiting = pane.locator('.jp-j[data-job][data-state="queued"]');
  await expect(waiting.nth(1)).toBeVisible();
  const failed = pane.locator('.jp-j[data-job][data-state="failed"]').first();
  // The failed one, then two waiting ones by their glyphs: Pause holds the waiting ones, not the failed.
  await failed.locator(".jp-line").click();
  await waiting.nth(0).locator(".jp-glyph").click();
  await waiting.nth(1).locator(".jp-glyph").click();
  await expect(strip(pane).locator(".jp-said")).toContainText("3 jobs picked");
  const ids = [await waiting.nth(0).getAttribute("data-job"), await waiting.nth(1).getAttribute("data-job")];
  await verb(pane, "Pause").click();
  await expect(strip(pane).locator(".jp-said")).toContainText("Paused");
  expect((await sent(page)).at(-1)).toEqual({ repo: "hyper-light/kiln", sha, verb: "pause", jobs: ids });
});

test("each word picks its jobs, and each action reaches the endpoint with what's picked", async ({ page }) => {
  const { pane, sha } = await open(page);
  const base = { repo: "hyper-light/kiln", sha };
  // failed: the failed job, Retry alone lit, and it asks for the word's jobs.
  await word(pane, "failed").click();
  await expect(word(pane, "failed")).toHaveAttribute("aria-pressed", "true");
  await expect(pane.locator('.jp-j[data-sel][data-state="failed"]')).toHaveCount(1);
  await expect(verb(pane, "Retry")).toBeEnabled();
  await expect(verb(pane, "Pause")).toBeDisabled();
  await expect(verb(pane, "Cancel")).toBeDisabled();
  await verb(pane, "Retry").click();
  await expect(strip(pane).locator(".jp-said")).toContainText("Run again");
  expect((await sent(page)).at(-1)).toEqual({ ...base, verb: "retry", scope: "failed" });
  // running: Pause freezes them; then the pick is paused and Pause says Resume.
  // The mock's live run has a job running and jobs waiting: each word has its own.
  await expect(pane.locator('.jp-j[data-job][data-state="running"]').first()).toBeVisible();
  {
    await word(pane, "running").click();
    await verb(pane, "Pause").click();
    await expect(strip(pane).locator(".jp-said")).toContainText("Paused");
    expect((await sent(page)).at(-1)).toEqual({ ...base, verb: "pause", scope: "running" });
    await expect(pane.locator('.jp-j[data-state="paused"]').first()).toBeVisible();
    // The line itself, picked alone: Resume, by its job.
    const paused = pane.locator('.jp-j[data-state="paused"]').first();
    const id = await paused.getAttribute("data-job");
    await paused.locator(".jp-line").click();
    await expect(verb(pane, "Resume")).toBeEnabled();
    await verb(pane, "Resume").click();
    await expect(strip(pane).locator(".jp-said")).toContainText("Resumed");
    expect((await sent(page)).at(-1)).toEqual({ ...base, verb: "resume", jobs: [id] });
  }
  // waiting: Cancel asks again; Cancel them stops them.
  await word(pane, "waiting").click();
  await expect(pane.locator(".jp-j[data-sel]").first()).toBeVisible();
  {
    await verb(pane, "Cancel").click();
    await expect(strip(pane).locator(".jp-said")).toContainText("Cancel stops");
    await verb(pane, "Cancel them").click();
    await expect(strip(pane).locator(".jp-said")).toContainText("Cancelled");
    expect((await sent(page)).at(-1)).toEqual({ ...base, verb: "cancel", scope: "waiting" });
  }
});

test("Cancel asks again in the strip, and Keep leaves everything as it was", async ({ page }) => {
  const { pane } = await open(page);
  await word(pane, "all").click();
  await expect(verb(pane, "Cancel")).toBeEnabled();
  const before = (await sent(page)).length;
  await verb(pane, "Cancel").click();
  await expect(verb(pane, "Keep")).toBeVisible();
  await expect(verb(pane, "Cancel them")).toBeVisible();
  await verb(pane, "Keep").click();
  await expect(verb(pane, "Retry")).toBeVisible();
  expect((await sent(page)).length).toBe(before);
});

test("what an action couldn't do says why in the strip", async ({ page }) => {
  const { pane } = await open(page);
  // Paused, all: the failed job has ended, and says so.
  await word(pane, "all").click();
  await verb(pane, /^(Pause|Resume)$/).click();
  await expect(strip(pane).locator(".jp-said")).toContainText("has finished");
  await expect(strip(pane).locator(".jp-say")).toHaveAttribute("data-tone", "warn");
});

test("nothing moves as jobs are picked, acted on and go on for 30 s", async ({ page, browserName }) => {
  test.skip(browserName !== "chromium", "layout shifts are Chromium's to count");
  test.setTimeout(90_000);
  const { pane } = await open(page);
  await page.evaluate(() => document.fonts.ready.then(() => undefined));
  await page.evaluate(() => {
    const w = window as unknown as { __shift: number; __moved: string[] };
    w.__shift = 0;
    w.__moved = [];
    new PerformanceObserver((l) => {
      for (const e of l.getEntries() as unknown as { value: number; hadRecentInput: boolean; sources?: { node?: Node }[] }[]) {
        if (e.hadRecentInput) continue;
        if (!(e.sources ?? []).some((s) => (s.node instanceof Element ? s.node : (s.node?.parentElement ?? null))?.closest(".ship"))) continue;
        w.__shift += e.value;
        for (const s of e.sources ?? []) w.__moved.push(`${s.node?.nodeName ?? "?"} "${(s.node?.textContent ?? "").slice(0, 40)}"`);
      }
    }).observe({ type: "layout-shift" });
  });
  for (const w of ["failed", "running", "waiting", "all"]) {
    await word(pane, w).click();
    await page.waitForTimeout(700);
  }
  // All picked: Pause holds those waiting and freezes the one running.
  await verb(pane, "Pause").click();
  await expect(strip(pane).locator(".jp-said")).toContainText("Paused");
  await page.waitForTimeout(30_000);
  const seen = await page.evaluate(() => ({ shift: (window as unknown as { __shift: number }).__shift, moved: (window as unknown as { __moved: string[] }).__moved }));
  expect(seen.shift, `what moved: ${seen.moved.join(" | ")}`).toBe(0);
});

for (const width of [320, 390, 1280]) {
  test(`at ${width}, the opened pane scrolls nothing sideways`, async ({ page, isMobile }) => {
    test.skip(isMobile, "each width is set here; the desktop projects run them");
    await page.setViewportSize({ width, height: 900 });
    const { pane } = await open(page);
    await word(pane, "all").click();
    await verb(pane, "Cancel").click();
    await expect(verb(pane, "Cancel them")).toBeVisible();
    expect(await page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth)).toBe(0);
    for (const sel of [".jp-strip", ".jp-verbs button", ".jp-list", ".jp-j"]) {
      const right = await pane.locator(sel).evaluateAll((els) => Math.max(...els.map((n) => n.getBoundingClientRect().right)));
      expect(right, sel).toBeLessThanOrEqual(width + 0.5);
    }
  });
}
