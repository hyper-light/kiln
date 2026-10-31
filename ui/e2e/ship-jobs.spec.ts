/**
 * An opened commit's job cards (plan 01a175b7f21b7455bcdcd71e94ecdce0, step 1), against the mock's
 * fixed rows: at 1280, 1000, 800, 600 and 390 the visible cards and their gaps span exactly the
 * lanes card's width (within 1 px), previous is off at the start, and nothing scrolls sideways
 * (W21-AC01, as ci/ui/layout.mjs asks it); paging shows each view in turn.
 */
import { expect, test } from "@playwright/test";

const REAL = !!process.env.E2E_BASE_URL;
test.skip(REAL, "drives the mock's commits");

// As many to a view as fit: six, four, three, two, one.
const PER_VIEW: Record<number, number> = { 1280: 6, 1000: 4, 800: 3, 600: 2, 390: 1 };

/**
 * What scrolls sideways, as W21-AC01's layout.mjs finds it: the page, or any element that would
 * scroll its content. Review's Files and Conversation are Review's to check (its code blocks).
 */
const sideways = (page: import("@playwright/test").Page) =>
  page.evaluate(() => {
    const out: string[] = [];
    const doc = document.scrollingElement;
    if (doc && doc.scrollWidth > window.innerWidth + 1) out.push(`the page (${doc.scrollWidth - window.innerWidth}px)`);
    for (const el of document.querySelectorAll("body *")) {
      if (el.closest('section[aria-label="Files"], section[aria-label="Conversation"]')) continue;
      const cs = getComputedStyle(el);
      if ((cs.overflowX === "auto" || cs.overflowX === "scroll") && el.clientWidth > 0 && el.scrollWidth > el.clientWidth + 1)
        out.push(`${el.tagName.toLowerCase()}.${el.className} (${el.scrollWidth - el.clientWidth}px)`);
    }
    return out;
  });

for (const width of [1280, 1000, 800, 600, 390]) {
  test(`at ${width}, the visible job cards span the lanes card's width`, async ({ page, isMobile }) => {
    test.skip(isMobile, "each width is set here; the desktop project runs them all");
    await page.setViewportSize({ width, height: 900 });
    await page.goto("/ship");
    const row = page.getByRole("list", { name: "Commits", exact: true }).locator(":scope > li").first();
    await row.getByRole("button", { name: /^Open / }).click();
    await expect(row.getByRole("list", { name: "Stages" })).toBeVisible();
    const m = await row.evaluate((li) => {
      const track = li.querySelector(".ship-jobs__track");
      const lanes = li.querySelector('section[aria-label="Lanes"]');
      if (!track || !lanes) return null;
      const t = track.getBoundingClientRect();
      const shown = [...track.children].map((c) => c.getBoundingClientRect()).filter((r) => r.left >= t.left - 1 && r.right <= t.right + 1);
      const first = shown[0];
      const last = shown[shown.length - 1];
      const cs = getComputedStyle(lanes);
      const inner = lanes.getBoundingClientRect().width - parseFloat(cs.paddingLeft) - parseFloat(cs.paddingRight) - parseFloat(cs.borderLeftWidth) - parseFloat(cs.borderRightWidth);
      return { n: shown.length, span: first && last ? last.right - first.left : 0, lanes: inner };
    });
    expect(m).not.toBeNull();
    expect(m?.n).toBe(PER_VIEW[width]);
    expect(Math.abs((m?.span ?? 0) - (m?.lanes ?? 0))).toBeLessThanOrEqual(1);
    await expect(row.getByRole("button", { name: "Previous stages" })).toBeDisabled();
    expect(await sideways(page)).toEqual([]);
  });
}

test("at 390, previous and next show each stage in turn, Review first, one view at a time, off at the ends", async ({ page, isMobile }) => {
  test.skip(isMobile, "the width is set here; the desktop projects run it");
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("/ship");
  const row = page.getByRole("list", { name: "Commits", exact: true }).locator(":scope > li").first();
  await row.getByRole("button", { name: /^Open / }).click();
  const cards = row.locator(".ship-jobs__track > .ship-stn");
  const next = row.getByRole("button", { name: "Next stages" });
  const prev = row.getByRole("button", { name: "Previous stages" });
  const seen: (string | null)[] = [];
  for (let i = 0; i < 7; i++) {
    await expect(cards).toHaveCount(1);
    seen.push(await cards.first().getAttribute("data-stage"));
    if (i < 6) await next.click();
  }
  expect(seen).toEqual(["review", "land", "checks", "build", "staging", "production", "replant"]);
  await expect(next).toBeDisabled();
  await prev.click();
  await expect(cards.first()).toHaveAttribute("data-stage", "production");
  expect(await sideways(page)).toEqual([]);
});

test("one job is picked at a time: the rule's first, then the one you click", async ({ page, isMobile }) => {
  test.skip(isMobile, "the desktop shows six cards at once");
  await page.goto("/ship");
  const row = page.getByRole("list", { name: "Commits", exact: true }).locator(":scope > li").first();
  await row.getByRole("button", { name: /^Open / }).click();
  const jobs = row.getByRole("list", { name: "Stages" }).getByRole("button");
  await expect(jobs.first()).toBeVisible({ timeout: 15_000 });
  await expect(row.locator('.ship-part[aria-pressed="true"]')).toHaveCount(1);
  await jobs.first().click();
  await expect(jobs.first()).toHaveAttribute("aria-pressed", "true");
  await expect(row.locator('.ship-part[aria-pressed="true"]')).toHaveCount(1);
});

/** A leg's state as its card draws its glyph, and its time as the card says it ("4m 20s"). */
const timeOf = (start?: number, end?: number) => {
  if (start == null || end == null || end <= start) return null;
  const s = Math.round(end - start);
  return s < 60 ? `${s}s` : s < 3600 ? `${Math.floor(s / 60)}m ${String(s % 60).padStart(2, "0")}s` : `${Math.floor(s / 3600)}h ${String(Math.floor(s / 60) % 60).padStart(2, "0")}m`;
};

test("each stage's card lists its jobs as f-ship does: Checks each leg with its state and time, and the job picked is the one Output shows", async ({ page, isMobile }) => {
  test.skip(isMobile, "the desktop shows six cards at once");
  test.setTimeout(90_000);
  await page.setViewportSize({ width: 1280, height: 900 });
  await page.goto("/ship");
  const row = page.getByRole("list", { name: "Commits", exact: true }).locator(":scope > li").first();
  const sha = await row.locator(".ship-sha").first().textContent();
  await row.getByRole("button", { name: /^Open / }).click();
  const checks = row.locator('.ship-stn[data-stage="checks"]');
  await expect(checks).toBeVisible({ timeout: 15_000 });
  // Review first, then every stage, each its head's word on the right.
  await expect(row.locator(".ship-jobs__track > .ship-stn")).toHaveCount(6);
  await expect(row.locator(".ship-jobs__track > .ship-stn").first()).toHaveAttribute("data-stage", "review");
  await expect(row.locator('.ship-stn[data-stage="staging"] .ship-part__name')).toHaveText(["sync", "soak"]);
  await expect(row.locator('.ship-stn[data-stage="production"] .ship-part__name')).toHaveText(["approval", "deploy", "guard"]);
  // Checks: each leg the read model has, as "pipeline · leg", its state's glyph and its time. The
  // mock's live run moves on as it's read: the page and the read model are compared until they agree.
  type Legs = { checks: { pipeline: string; legs: { name: string; state: string; job?: string; start?: number; end?: number }[] }[] };
  const read = async () =>
    ((await (await page.request.get(`/api/ship/commits/hyper-light/kiln/${sha?.trim() ?? ""}`)).json()) as Legs).checks.flatMap((c) =>
      c.legs.map((l) => ({ ...l, shown: `${c.pipeline} · ${l.name}` })),
    );
  const draw = () =>
    checks
      .locator(".ship-part")
      .evaluateAll((els) =>
        els.map((b) => ({
          name: b.querySelector(".ship-part__name")?.textContent ?? "",
          state: b.querySelector("svg.sg")?.getAttribute("data-state") ?? "",
          v: b.querySelector(".ship-part__v")?.textContent ?? "",
        })),
      );
  let legs = await read();
  expect(legs.length).toBeGreaterThan(1);
  await expect
    .poll(
      async () => {
        legs = await read();
        const drawn = await draw();
        if (drawn.length !== legs.length) return [`${drawn.length} drawn of ${legs.length}`];
        return legs.flatMap((l, i) => {
          const d = drawn[i];
          const t = timeOf(l.start, l.end);
          return d?.name === l.shown && d.state === l.state && (!t || d.v === t) ? [] : [`${l.shown}: ${l.state} ${t ?? ""}, drawn ${JSON.stringify(d)}`];
        });
      },
      { timeout: 30_000 },
    )
    .toEqual([]);
  await expect(checks.locator(".ship-stn__n")).toHaveText(`${legs.filter((l) => l.state === "passed").length}/${legs.length}`);
  // Each job that ran, picked, is the one Output shows.
  for (const l of legs.filter((x) => x.job)) {
    await checks.getByRole("button", { name: l.shown }).click();
    await expect(checks.getByRole("button", { name: l.shown })).toHaveAttribute("aria-pressed", "true");
    await expect(row.locator(".ship-output").first()).toHaveAttribute("data-job", l.job ?? "", { timeout: 15_000 });
  }
});
