/**
 * Land beside Approve in a Ship item's gate row (docs/ci.md W26-R19): while Review's gate holds a
 * change, Land when ready has Review hand it to the queue once it clears, and then it says so,
 * disabled. Land keeps its place and width whatever it says, inside the row, and its icon holds still as
 * the queue moves it on with no one at the page (no layout shift). The mock (mock/review.ts) holds D41 on
 * kim's request for changes. Chromium and WebKit, desktop and phone.
 */
import { expect, test } from "@playwright/test";
import { pickCommit } from "./ship-pick";

test("a held change asks to land when ready, then says so, in its place", async ({ page }) => {
  // Each project starts from D41 not yet asked to land (the mock is shared by every project).
  const unask = () => page.request.post("/api/diffs/D41/land", { data: { when_ready: false } });
  await unask();
  await page.goto("/ship");
  const row = await pickCommit(page, "pipeline: dynamic matrices");
  const bar = row.locator(".rv-bar").first();
  const land = bar.locator(".rv-land");
  await expect(land).toHaveText("Land when ready");
  await expect(land).toHaveAttribute("title", /kim requested changes/);
  await expect(land).toBeEnabled();
  const before = await land.boundingBox();
  await land.click();
  await expect(land).toHaveText("Lands when ready");
  await expect(land).toBeDisabled();
  const [after, b] = await Promise.all([land.boundingBox(), bar.boundingBox()]);
  expect(before && after && b).toBeTruthy();
  if (before && after && b) {
    expect(Math.abs(after.x - before.x)).toBeLessThanOrEqual(1);
    expect(Math.abs(after.width - before.width)).toBeLessThanOrEqual(1);
    expect(after.x + after.width).toBeLessThanOrEqual(b.x + b.width + 1);
  }
  await unask();
});

test("Land's icon holds still as the queue moves the change on, with no one at the page", async ({ page, browserName }) => {
  const move = (state: string | null) => page.request.post("/api/mock/review", { data: { queue: { diff: "D41", state } } });
  await move(null);
  await page.goto("/ship");
  const row = await pickCommit(page, "pipeline: dynamic matrices");
  const land = row.locator(".rv-bar").first().locator(".rv-land");
  await expect(land).toHaveText("Land when ready");
  // What's measured is the queue's words arriving, so the fonts are in first (a late swap isn't one).
  await page.evaluate(() => document.fonts.ready.then(() => undefined));
  if (browserName === "chromium")
    await page.evaluate(() => {
      const w = window as unknown as { __shift: number; __moved: string[] };
      w.__shift = 0;
      w.__moved = [];
      new PerformanceObserver((l) => {
        for (const e of l.getEntries() as unknown as { value: number; hadRecentInput: boolean; sources?: { node?: Node; previousRect: DOMRectReadOnly; currentRect: DOMRectReadOnly }[] }[]) {
          if (e.hadRecentInput) continue;
          // Ship's own, as ship-perf.spec counts them: the app's approval notices come and go on their own.
          if (!(e.sources ?? []).some((s) => (s.node instanceof Element ? s.node : (s.node?.parentElement ?? null))?.closest(".ship"))) continue;
          w.__shift += e.value;
          for (const s of e.sources ?? []) {
            const n = s.node as HTMLElement | undefined;
            w.__moved.push(
              `${n?.nodeName ?? "?"}.${typeof n?.className === "string" ? n.className.slice(0, 50) : ""} "${(n?.textContent ?? "").slice(0, 30)}" x${Math.round(s.previousRect.x)}->${Math.round(s.currentRect.x)}`,
            );
          }
        }
      }).observe({ type: "layout-shift" });
    });
  const icon = land.locator("svg");
  const at = await icon.boundingBox();
  for (const [state, word] of [
    ["queued", "In the queue"],
    ["testing", "Testing"],
    ["landed", "Landed"],
  ] as const) {
    await move(state);
    await expect(land).toHaveText(word);
    const now = await icon.boundingBox();
    expect(at && now).toBeTruthy();
    if (at && now) expect([now.x - at.x, now.y - at.y], word).toEqual([0, 0]);
  }
  if (browserName === "chromium") {
    const seen = await page.evaluate(() => ({ shift: (window as unknown as { __shift: number }).__shift, moved: (window as unknown as { __moved: string[] }).__moved }));
    expect(seen.shift, `what moved: ${seen.moved.join(" | ")}`).toBe(0);
  }
  await move(null);
});
