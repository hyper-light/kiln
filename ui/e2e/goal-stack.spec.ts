/**
 * A goal stack's work at once (docs/goals.md, "A plan's steps side by side"; docs/agentic.md P2),
 * in the mock's "Replication lead": a planned goal whose steps 2, 3 and 4 run side by side on
 * subagents of its own (3's asking you something), and a goal of its own out on a subagent. Every
 * line and step at work moves, each its own way, the one asking you says so, a goal out on a
 * subagent isn't offered to plan here, and nothing scrolls sideways. On a desktop and a phone.
 * And goals under way on an agent whose turn is over while a subagent it sent off still works
 * (docs/agentic.md G2): they turn as while its turn runs, and hold once none works.
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

/** Goals under way and one to come, as the server sends an agent's stack: the goal it's held to, and one it works through its subagents (`goals_update` active). */
const underWay = (agent: string) => ({
  agent,
  state: "running",
  updated_at: Date.now(),
  plan_big: true,
  items: [
    { id: "g-figures", text: "Bring the System page's figures up to date", status: "active", how: "goal", started_at: Date.now() - 20 * 60_000, reminded_at: Date.now() - 20 * 60_000 },
    { id: "g-logs", text: "Check the build logs against build 412", status: "active", started_at: Date.now() - 15 * 60_000 },
    { id: "g-ship", text: "Ship the page", status: "pending" },
  ],
});

test("G2: its goals under way turn while it works, its turn over too while a subagent it sent off still works; with none at work they hold", async ({ page }, info) => {
  // The same stack on three of the mock's agents: "System page", its turn over, a subagent still at work (a shell and a monitor
  // left running beside it); "Release notes", its turn over, its subagents done; "Fix the flaky retry test", its turn under way.
  for (const [agent, stand] of [
    ["sub-busy", "working"],
    ["sub-done", "waiting"],
    ["fam-tests", "working"],
  ] as const) {
    await page.route(`**/api/harness/sessions/${agent}/goals`, (route) => route.fulfill({ json: underWay(agent) }));
    await page.goto(`/agents/${agent}?tab=plan`);
    const nodes = page.locator(".gs-chain .gs-node");
    await expect(nodes).toHaveCount(3);
    await expect(nodes.nth(0), agent).toHaveAttribute("data-stand", stand);
    await expect(nodes.nth(1), agent).toHaveAttribute("data-stand", stand);
    await expect(nodes.nth(2), agent).toHaveAttribute("data-stand", "waiting");
    // At work, each ring's light turns; between turns with none at work, it holds in the light's blue.
    const turns = await nodes.evaluateAll((ns) => ns.map((n) => getComputedStyle(n, "::before").animationName));
    expect(turns, agent).toEqual(stand === "working" ? ["gs-turn", "gs-turn", "none"] : ["none", "none", "none"]);
    if (stand === "waiting") await expect(nodes.nth(0), agent).toHaveCSS("border-top-color", "rgb(166, 196, 237)");
    // A still of it, with room for its rings' light, the cards of other agents waiting on you out of the way.
    await page.locator(".gs-chain").scrollIntoViewIfNeeded();
    const box = await page.locator(".gs-chain").boundingBox();
    if (box) {
      const clip = { x: Math.max(0, box.x - 8), y: Math.max(0, box.y - 8), width: box.width + 16, height: box.height + 16 };
      await page.screenshot({ path: info.outputPath(`goal-stack-${agent}-${info.project.name}.png`), clip, style: ".rq-viewport { visibility: hidden; }" });
    }
  }
});
