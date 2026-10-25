/**
 * A goal stack's work at once (docs/goals.md, "A plan's steps side by side"; docs/agentic.md P2),
 * in the mock's "Replication lead": a planned goal whose steps 2, 3 and 4 run side by side on
 * subagents of its own (3's asking you something), and a goal of its own out on a subagent. Every
 * line and step at work moves, each its own way, the one asking you says so, a goal out on a
 * subagent isn't offered to plan here, and nothing scrolls sideways. On a desktop and a phone.
 */
import { expect, test } from "@playwright/test";

test("work at once moves on every line and step it's on, each its own way", async ({ page }) => {
  await page.goto("/agents/fan-lead?tab=plan");
  const stack = page.locator(".gs-chain");
  await expect(stack.locator(".gp-row[data-stand='working']")).toHaveCount(2);
  await expect(stack.locator(".gp-row[data-stand='needs_you']")).toHaveCount(1);
  await expect(stack.locator(".gs-node[data-stand='working']")).toHaveCount(2);
  const seen = await page.evaluate(() => {
    const style = (el: Element, pseudo?: string) => getComputedStyle(el, pseudo);
    return {
      // Each goal at work: its ring's light turning.
      turning: [...document.querySelectorAll(".gs-node[data-stand='working']")].map((n) => style(n, "::before").animationName),
      // Each step at work: its glyph's light turning.
      beams: document.querySelectorAll(".gp-row[data-stand='working'] .sg-trace[data-status='running']").length,
      // Each one's words in the shimmer, its own way: the two goals' and the two steps'.
      flows: [...document.querySelectorAll(".gs-chain .sg-shimmer")].map((s) => `${style(s).animationName} ${style(s).animationDuration} ${style(s).animationDelay}`),
      wide: (document.scrollingElement?.scrollWidth ?? 0) > window.innerWidth,
    };
  });
  expect(seen.turning).toEqual(["gs-turn", "gs-turn"]);
  expect(seen.beams).toBe(2);
  expect(seen.flows).toHaveLength(4);
  expect(new Set(seen.flows).size, `none in step: ${seen.flows.join(" | ")}`).toBe(4);
  expect(seen.wide).toBe(false);
  await expect(stack.getByRole("button", { name: "Make a plan" })).toHaveCount(0);
});
