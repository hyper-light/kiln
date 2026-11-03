/**
 * A plan's steps out on subagents, in the Plan tab's goal stack (goal-plan.tsx), in the production
 * build against the mock (mock/goals.ts, tree-lead): each step's agent live under it (who, how long,
 * what it's doing; its thread from there), and the step that needs you with its request answered
 * there and its stretch of the path tan.
 */
import { expect, test, type Locator, type Page } from "@playwright/test";

/** Against a real kiln server (playwright.config.ts): these drive the mock's agents. */
const REAL = !!process.env.E2E_BASE_URL;

/** The colour its stretch of the path is drawn in. */
const stretch = (row: Locator) => row.evaluate((el) => getComputedStyle(el, "::before").backgroundColor);

async function plan(page: Page) {
  await page.goto("/agents/tree-lead?tab=plan");
  return page.getByRole("list", { name: "Plan “Replicated log”" }).first();
}

test.beforeEach(async ({ page }) => {
  test.skip(REAL, "the mock's agents");
  await page.request.post("/api/mock/tree-asks");
});

test.afterAll(async ({ request }) => {
  if (!REAL) await request.post("/api/mock/asks");
});

test("each step out on a subagent shows its agent live: who, how long and what it's doing; its thread opens from there", async ({ page }) => {
  const steps = await plan(page);
  const election = steps.locator(".gp-row", { hasText: "Leader election over QUIC" });
  await expect(election.locator(".gp-who")).toContainText("Claude Code · metal-ryzen-01 · ");
  await expect(election.locator(".gp-live")).toHaveText("Running cargo test -p hyper-raft election");
  await election.locator(".gp-who").click();
  await expect(page).toHaveURL(/\/agents\/tree-lead\?task=tree-s1$/);
});

test("the step that needs you: its request under it, answered there, and its stretch of the path tan until it is", async ({ page }) => {
  const steps = await plan(page);
  const catchUp = steps.locator(".gp-row", { hasText: "Follower catch-up" });
  await expect(catchUp).toHaveAttribute("data-stand", "needs_you");
  await expect(catchUp.locator(".gp-live[data-ask]")).toHaveText("Bash: kiln run --on linux/amd64 -- cargo test -p hyper-raft catch_up");
  expect(await stretch(catchUp)).toBe("rgb(198, 176, 142)");
  // The other steps keep the path's own colour.
  expect(await stretch(steps.locator(".gp-row", { hasText: "Snapshot transfer" }))).not.toBe("rgb(198, 176, 142)");
  await catchUp.getByRole("button", { name: "Allow" }).click();
  await expect(catchUp).toHaveAttribute("data-stand", "working");
  expect(await stretch(catchUp)).not.toBe("rgb(198, 176, 142)");
});
