/**
 * A message the person writes to one of an agent's subagents that its harness takes only by way of
 * the agent (Claude Code's SendMessage, Codex's agent tools) goes through the agent as kiln asks it,
 * but it isn't the agent's conversation with the person: what kiln wrote the agent, the call passing
 * it on, and a turn that did nothing else don't show in the main thread; the message shows in the
 * subagent's thread. In the production build against the mock (mock/agents.ts, relay-lead).
 */
import { expect, test } from "@playwright/test";

/** Against a real kiln server (playwright.config.ts): this reads the mock's agent. */
const REAL = !!process.env.E2E_BASE_URL;

test("what kiln asked the agent to pass on to its subagent isn't in their conversation; the message is in the subagent's thread", async ({ page }) => {
  test.skip(REAL, "the mock's agents");
  await page.goto("/agents/relay-lead");
  const main = page.locator(".hx-flow");
  await expect(main).toContainText("A subagent is on the tests.");
  // Its own work, in the turn a relay was steered into, shows.
  await expect(main).toContainText("Version bumped to 0.3.0.");
  await expect(main).not.toContainText("The person wrote a message to your subagent");
  await expect(main).not.toContainText("Passing that on to the subagent.");
  await expect(main).not.toContainText("SendMessage");
  await expect(page.locator('.hx-prompt[data-origin="kiln"]')).toHaveCount(0);

  await page.goto("/agents/relay-lead?task=a575c1d3e5f7a9b1c");
  const thread = page.locator(".hx-flow");
  await expect(thread).toContainText("Use the v2 parser");
  await expect(thread).toContainText("Then run the figure tests");
});
