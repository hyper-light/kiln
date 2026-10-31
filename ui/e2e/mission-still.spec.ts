/**
 * Mission control comes in still (docs/agents.md, "Mission control"): while its agents load, the page
 * shows its own shape (the stands' line, the composer with what's written in it, the filter and the
 * list's head over a few rows), each part where the page puts it. The page then comes whole, with
 * what only the answer can tell (a note under the composer, a notice over the list), and once the
 * rows show nothing over them moves. Every part's box every frame from the first, in the production
 * build against the mock, on a desktop and a phone.
 */
import { expect, test, type Page } from "@playwright/test";

/** Against a real kiln server (playwright.config.ts): these drive the mock's agents. */
const REAL = !!process.env.E2E_BASE_URL;

type Box = { top: number; height: number };
/** A frame: whether the page was still its skeleton, and each part's box in the page (null: not drawn). */
type Frame = { at: number; skeleton: boolean; box: Record<string, Box | null> };
const PARTS = [".sd", ".mc-mode", ".hx-compose-box", ".mc-under", ".ms-bar", ".ms-listhead", "li.ms-row"];

/** Opens Mission control, its agents a moment late (so its skeleton shows), and records every part's box every frame until it has been whole a while. */
async function framesOf(page: Page): Promise<Frame[]> {
  await page.route("**/api/harness/sessions", async (route) => {
    await new Promise((r) => setTimeout(r, 400));
    await route.continue();
  });
  await page.addInitScript((parts: string[]) => {
    const w = window as unknown as { mcFrames: Frame[] };
    w.mcFrames = [];
    const t0 = performance.now();
    const step = () => {
      const box: Record<string, Box | null> = {};
      for (const p of parts) {
        const r = document.querySelector(p)?.getBoundingClientRect();
        box[p] = r ? { top: r.top + window.scrollY, height: r.height } : null;
      }
      w.mcFrames.push({ at: performance.now() - t0, skeleton: !!document.querySelector(".ms-loading"), box });
      if (performance.now() - t0 < 5000) requestAnimationFrame(step);
    };
    requestAnimationFrame(step);
  }, PARTS);
  await page.goto("/agents");
  await expect(page.locator(".ms-loading")).toHaveCount(0);
  // A while whole: what refreshes on its own (the agents' news, the machines' and runners' reads) has had its turn.
  await page.waitForTimeout(2500);
  const frames = await page.evaluate(() => (window as unknown as { mcFrames: Frame[] }).mcFrames);
  expect(
    frames.some((f) => f.skeleton && f.box["li.ms-row"]),
    "the skeleton was never drawn",
  ).toBe(true);
  return frames;
}

/** Every frame that drew `part` somewhere else than the first frame that drew it did (by more than a sliver of a pixel). */
function moved(frames: Frame[], part: string, of: (b: Box) => number = (b) => b.top): string[] {
  const drawn = frames.flatMap((f) => (f.box[part] ? [{ at: f.at, v: of(f.box[part]) }] : []));
  const first = drawn[0];
  if (!first) return ["never drawn"];
  return drawn.filter((d) => Math.abs(d.v - first.v) > 0.5).map((d) => `${d.at.toFixed(0)} ms: ${d.v.toFixed(1)}, first ${first.v.toFixed(1)}`);
}

test("with nothing only the answer can tell, Mission control comes in still: its skeleton is the page's shape", async ({ page }) => {
  test.skip(REAL, "the mock's agents");
  // Every connected machine's runner current (none too old to run agents), and no notifications on offer.
  await page.route("**/api/push", (route) => route.fulfill({ json: { enabled: false, available: false, why: "Not here", public_key: null, kinds: [], muted: [], reached: null } }));
  await page.route("**/api/harness/kinds", async (route) => {
    const res = await route.fetch();
    const kinds = (await res.json()) as Record<string, unknown[]>;
    const machines = (await (await page.request.get("/api/agents")).json()) as { name: string; connected: boolean }[];
    for (const m of machines) if (m.connected) kinds[`agent:${m.name}`] ??= kinds.local ?? [];
    await route.fulfill({ response: res, json: kinds });
  });
  const frames = await framesOf(page);
  for (const part of PARTS) expect(moved(frames, part), `${part} moved`).toEqual([]);
  // The rows are the agents', not the skeleton's: their heights are their own.
  for (const part of PARTS.filter((p) => p !== "li.ms-row"))
    expect(
      moved(frames, part, (b) => b.height),
      `${part} changed height`,
    ).toEqual([]);
});

test("what only the answer can tell comes with the page, and once the rows show nothing over them moves", async ({ page }) => {
  test.skip(REAL, "the mock's agents");
  // The mock as it is: machines whose runners are too old (a note under the composer), and notifications on offer where this browser takes them.
  const frames = await framesOf(page);
  // The stands' line and the composer never move: its box and More options stay where the skeleton drew them.
  for (const part of [".sd", ".mc-mode", ".hx-compose-box", ".mc-under"]) expect(moved(frames, part), `${part} moved`).toEqual([]);
  for (const part of [".sd", ".hx-compose-box", ".mc-under"])
    expect(
      moved(frames, part, (b) => b.height),
      `${part} changed height`,
    ).toEqual([]);
  // The note comes with the page, under the composer: the filter moves by its room, then never again.
  const skeleton = frames.filter((f) => f.skeleton && f.box[".ms-bar"]).at(-1);
  const whole = frames.filter((f) => !f.skeleton && f.box["li.ms-row"]);
  const first = whole[0];
  expect(skeleton && first, "no skeleton and page to compare").toBeTruthy();
  if (!skeleton || !first) return;
  const sk = (p: string) => skeleton.box[p] ?? { top: NaN, height: NaN };
  const pg = (p: string) => first.box[p] ?? { top: NaN, height: NaN };
  const note = pg(".mc-mode").height - sk(".mc-mode").height;
  expect(note, "the mock's note under the composer").toBeGreaterThan(0);
  expect(Math.abs(pg(".ms-bar").top - sk(".ms-bar").top - note), "the filter moved by more than the note's room").toBeLessThan(0.5);
  for (const part of [".mc-mode", ".ms-bar", ".ms-listhead", "li.ms-row"]) expect(moved(whole, part), `${part} moved once the rows showed`).toEqual([]);
  expect(
    moved(whole, ".mc-mode", (b) => b.height),
    "the composer changed height once the rows showed",
  ).toEqual([]);
});
