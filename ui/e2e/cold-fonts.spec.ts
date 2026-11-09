/** W21-R02/R13: late fonts do not block a session or replace its composer/reading position. */
import { expect, test } from "@playwright/test";

test("W21-R02 late fonts preserve the draft, focus, transcript anchor and normal entrance animation", async ({ page }, info) => {
  test.skip(!!process.env.E2E_BASE_URL, "the mock's fixed long conversation");
  await page.emulateMedia({ reducedMotion: "no-preference" });
  let release = () => {};
  const held = new Promise<void>((resolve) => {
    release = resolve;
  });
  let requests = 0;
  await page.route(/\.woff2(?:\?|$)/, async (route) => {
    requests++;
    await held;
    await route.continue();
  });
  await page.addInitScript(() => {
    const diag = {
      shifts: [] as { at: number; value: number }[],
      animations: [] as string[],
      supported: PerformanceObserver.supportedEntryTypes.includes("layout-shift"),
    };
    Object.assign(window, { __fontDiag: diag });
    if (diag.supported)
      new PerformanceObserver((entries) => {
        for (const entry of entries.getEntries())
          diag.shifts.push({
            at: entry.startTime,
            value: (entry as PerformanceEntry & { value: number }).value,
          });
      }).observe({ type: "layout-shift", buffered: true });
    document.addEventListener("animationstart", (event) => {
      if (event.animationName === "ag-ready" && (event.target as Element)?.classList.contains("hx-agent")) diag.animations.push(event.animationName);
    });
  });
  try {
    await page.goto("/agents/demo-long", { waitUntil: "domcontentloaded" });
    const field = page.getByRole("textbox", {
      name: "Message to the agent",
      exact: true,
    });
    await expect(field).toBeEnabled({ timeout: 30_000 });
    await expect.poll(() => requests).toBeGreaterThanOrEqual(2);
    await expect(page.locator('link[rel="preload"][as="font"]')).toHaveCount(2);
    await expect(page.locator("html")).toHaveAttribute("data-motion", "playing");
    await expect(page.locator(".hx-agent.ag-ready")).toHaveCSS("animation-duration", "0.18s");
    await expect.poll(() => page.evaluate(() => (window as unknown as { __fontDiag: { animations: string[] } }).__fontDiag.animations)).toContain("ag-ready");
    await field.fill("A draft while both console fonts are delayed.");
    await field.focus();
    await field.evaluate((el: HTMLTextAreaElement) => el.setSelectionRange(8, 13));
    const original = await field.elementHandle();
    const timeline = page.locator(".hx-timeline").first();
    const anchor = await timeline.evaluate((root) => {
      root.scrollTop = Math.max(0, (root.scrollHeight - root.clientHeight) / 2);
      root.dispatchEvent(new Event("scroll"));
      const top = root.getBoundingClientRect().top;
      const part = [...root.querySelectorAll<HTMLElement>(".hx-turn > [data-ev], .hx-work > [data-ev]")].find((el) => el.getBoundingClientRect().bottom > top + 1);
      return part
        ? {
            id: part.getAttribute("data-ev")!,
            offset: part.getBoundingClientRect().top - top,
          }
        : null;
    });
    expect(anchor, "the delayed-font conversation already has readable history").not.toBeNull();
    const releasedAt = await page.evaluate(() => performance.now());
    release();
    await page.evaluate(() => document.fonts.ready.then(() => new Promise<void>((done) => requestAnimationFrame(() => requestAnimationFrame(() => done())))));
    await expect(field).toBeFocused();
    await expect(field).toHaveValue("A draft while both console fonts are delayed.");
    expect(await field.evaluate((el, before) => el === before, original)).toBe(true);
    expect(await field.evaluate((el: HTMLTextAreaElement) => [el.selectionStart, el.selectionEnd])).toEqual([8, 13]);
    await expect
      .poll(() =>
        timeline.evaluate((root, saved) => {
          const part = [...root.querySelectorAll<HTMLElement>("[data-ev]")].find((el) => el.getAttribute("data-ev") === saved!.id);
          return part ? Math.abs(part.getBoundingClientRect().top - root.getBoundingClientRect().top - saved!.offset) : Infinity;
        }, anchor),
      )
      .toBeLessThanOrEqual(2);
    const shifts = await page.evaluate((at) => {
      const diag = (
        window as unknown as {
          __fontDiag: {
            supported: boolean;
            shifts: { at: number; value: number }[];
          };
        }
      ).__fontDiag;
      return {
        supported: diag.supported,
        value: diag.shifts.filter((e) => e.at >= at).reduce((sum, e) => sum + e.value, 0),
      };
    }, releasedAt);
    await info.attach("late-font-layout", {
      body: JSON.stringify(shifts),
      contentType: "application/json",
    });
    if (shifts.supported) expect(shifts.value, "font-swap layout shifts, including recent-input entries").toBeLessThanOrEqual(0.1);
  } finally {
    release();
    await page.unrouteAll({ behavior: "wait" });
  }
});
