/**
 * Agents' pace (Agent pace and checks plan 01a175b7f2837279be2a229ee94ac1f0, step 1), against the
 * mock: 14 agents' commits land on /ship in 10 s (`POST /api/mock/ship/agents`, each starting CI as
 * a push does), and the page keeps up: 55 frames a second or more, counted from Chromium's trace at
 * 390 px, with the rows that need you first and the commits riding with the release folded under
 * it, at every change of the list and at the end.
 */
import { expect, test } from "@playwright/test";

const REAL = !!process.env.E2E_BASE_URL;
test.skip(REAL, "drives the mock's commits");

/** A trace event, as much of one as is read here. */
interface TraceEvent {
  name: string;
  ph: string;
  ts: number;
  pid: number;
  args?: { data?: { message?: string }; frame_reporter?: { state?: string; frame_type?: string } };
}

/** What the page keeps while the burst lands: its frames, the longest wait for one, and each time the list read wrong. */
interface Burst {
  on: boolean;
  frames: number;
  gap: number;
  last: number;
  wrong: string[];
}

test("14 agents' commits in 10 s: 55 frames a second or more at 390 px, needs-you rows first, riders folded", async ({ page, browser, browserName, isMobile }) => {
  test.skip(browserName !== "chromium" || isMobile, "Chromium's trace; the desktop project sets 390 px");
  test.setTimeout(90_000);
  await page.request.post("/api/mock/ship/arrive?clear=1");
  try {
    await page.setViewportSize({ width: 390, height: 844 });
    await page.goto("/ship");
    const list = page.getByRole("list", { name: "Commits", exact: true });
    const rows = list.locator(":scope > li.ship-row");
    await expect(rows.first()).toBeVisible({ timeout: 15_000 });
    // A pixel changed every frame, on top of everything (one under the shell's bar changes nothing on
    // screen): an idle page draws nothing, so without it its frames would count nothing; with it, a
    // frame the page's own work holds up shows in the trace as not presented. And at every change of
    // the list, whether it still reads right.
    await page.evaluate(() => {
      const w = window as unknown as { burst: Burst };
      w.burst = { on: false, frames: 0, gap: 0, last: 0, wrong: [] };
      const px = document.createElement("div");
      px.style.cssText = "position:fixed;left:0;top:0;z-index:2147483647;width:1px;height:1px;pointer-events:none;background:#808080";
      document.body.append(px);
      const tick = (t: number) => {
        const b = w.burst;
        if (b.on) {
          b.frames++;
          if (b.last) b.gap = Math.max(b.gap, t - b.last);
          b.last = t;
        }
        px.style.opacity = px.style.opacity === "0.5" ? "0.6" : "0.5";
        requestAnimationFrame(tick);
      };
      requestAnimationFrame(tick);
      const ul = document.querySelector(".ship-list");
      if (!ul) throw new Error("no list");
      const check = () => {
        const heads = [...ul.querySelectorAll<HTMLElement>(":scope > li.ship-row")];
        const stands = heads.map((r) => r.dataset.stand ?? "");
        const other = stands.findIndex((s) => s !== "needs_you");
        if (other >= 0 && stands.slice(other).includes("needs_you")) w.burst.wrong.push(`a row that needs you after one that doesn't: ${stands.join(" ")}`);
        const alone = heads.map((r) => r.querySelector(":scope > .ship-r .ship-name")?.textContent ?? "").filter((name) => name.startsWith("agent: rider"));
        if (alone.length > 0) w.burst.wrong.push(`riders on rows of their own: ${alone.join(", ")}`);
      };
      new MutationObserver(check).observe(ul, { childList: true });
      new MutationObserver(check).observe(ul, { subtree: true, attributes: true, attributeFilter: ["data-stand"] });
    });
    await browser.startTracing(page, { categories: ["devtools.timeline", "disabled-by-default-devtools.timeline.frame"] });
    await page.evaluate(() => {
      console.timeStamp("burst");
      (window as unknown as { burst: Burst }).burst.on = true;
    });
    await page.request.post("/api/mock/ship/agents?n=14&over=10");
    // All 14 drawn: the release with its ten riders folded under it, and the three agents asking.
    const release = rows.filter({ hasText: "release: production waits on you" });
    await expect(release.locator(".ship-fold")).toHaveText("Show 10 commits riding with it", { timeout: 30_000 });
    await expect(rows.filter({ hasText: "agent: asks about flake" })).toHaveCount(3);
    const meter = await page.evaluate(() => {
      console.timeStamp("burst-end");
      const b = (window as unknown as { burst: Burst }).burst;
      b.on = false;
      return { frames: b.frames, gap: Math.round(b.gap), wrong: b.wrong };
    });
    const { traceEvents } = JSON.parse((await browser.stopTracing()).toString()) as { traceEvents: TraceEvent[] };
    const stamp = (message: string) => traceEvents.find((e) => e.name === "TimeStamp" && e.args?.data?.message === message);
    const start = stamp("burst");
    const end = stamp("burst-end");
    if (!start || !end) throw new Error("the trace has no burst stamps");
    const from = start.ts;
    const to = end.ts;
    // Each frame the page's compositor began (in the page's own process, where its stamps are: the
    // browser's window draws frames of its own) and what became of it: presented with everything the
    // page drew for it, or held up (presented without the page's part, or dropped). A frame with a
    // compositor animation in it is reported twice, the copy marked forked: it's counted once.
    const frames = traceEvents.filter((e) => e.name === "PipelineReporter" && e.ph === "b" && e.pid === start.pid && e.args?.frame_reporter?.frame_type !== "FORKED" && e.ts >= from && e.ts <= to);
    const presented = frames.filter((e) => e.args?.frame_reporter?.state === "STATE_PRESENTED_ALL").length;
    const seconds = (to - from) / 1e6;
    const fps = presented / seconds;
    const said = `${fps.toFixed(1)} fps: ${presented} of ${frames.length} frames presented in ${seconds.toFixed(2)} s; the page's own count ${meter.frames} (${(meter.frames / seconds).toFixed(1)}/s), its longest frame ${meter.gap} ms`;
    test.info().annotations.push({ type: "burst", description: said });
    console.log(`burst: ${said}`);
    expect(seconds, "the trace spans the burst").toBeGreaterThanOrEqual(10);
    expect(fps, said).toBeGreaterThanOrEqual(55);
    expect(meter.wrong).toEqual([]);
    // At the end too: the rows that need you first, the burst's four among them; the riders under the release.
    const stands = await rows.evaluateAll((els) => els.map((e) => (e as HTMLElement).dataset.stand ?? ""));
    const other = stands.findIndex((s) => s !== "needs_you");
    expect(other, stands.join(" ")).toBeGreaterThanOrEqual(4);
    expect(stands.slice(other), stands.join(" ")).not.toContain("needs_you");
    await expect(rows.locator(":scope > .ship-r .ship-name", { hasText: "agent: rider" })).toHaveCount(0);
    await release.locator(".ship-fold").click();
    const riders = release.locator("li.ship-rider");
    await expect(riders).toHaveCount(10);
    await expect(riders.locator(".ship-name")).toHaveText(
      Array.from({ length: 14 }, (_, i) => i + 1)
        .filter((k) => k > 1 && k % 4 !== 0)
        .map((k) => `agent: rider ${k}`)
        .reverse(),
    );
  } finally {
    await page.request.post("/api/mock/ship/arrive?clear=1");
  }
});
