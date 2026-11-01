/**
 * A subagent's thread: its whole path in its crumbs, each earlier name opening its own thread, and
 * pause, branch and its permissions on that line; its parent's messages under the parent's name,
 * with the way back; the person's "via" the agent that passes them on; and a finished one's end,
 * with the way back to its parent's. In the production build against the mock (mock/agents.ts:
 * relay-lead, the tree-* agents).
 */
import { expect, test, type Locator } from "@playwright/test";

/** Against a real kiln server (playwright.config.ts): these read the mock's agents. */
const REAL = !!process.env.E2E_BASE_URL;

const names = (page: import("@playwright/test").Page) => page.getByRole("navigation", { name: "Path" }).locator(".hx-crumb, .hx-here");

test.beforeEach(() => {
  test.skip(REAL, "the mock's agents");
});

test("a nested subagent's crumbs name its whole path, each earlier name opening its thread, with pause, branch and permissions on the line", async ({ page }) => {
  await page.goto("/agents/tree-s1?own=1&task=ts1-b");
  const path = page.getByRole("navigation", { name: "Path" });
  await expect(names(page)).toHaveText(["Storage lead", "Leader election over QUIC", "Measure election time"]);
  await expect(path.getByRole("link", { name: "Storage lead" })).toHaveAttribute("href", "/agents/tree-lead");
  await expect(path.getByRole("link", { name: "Leader election over QUIC" })).toHaveAttribute("href", "/agents/tree-lead?task=tree-s1");
  const line = page.locator(".hx-crumbs");
  for (const name of ["Pause", "Branch", "Permissions", "Back to the main thread"]) await expect(line.getByRole("button", { name, exact: true })).toHaveCount(1);

  await path.getByRole("link", { name: "Leader election over QUIC" }).click();
  await expect(page).toHaveURL(/\/agents\/tree-lead\?task=tree-s1$/);
  await expect(names(page)).toHaveText(["Storage lead", "Leader election over QUIC"]);
  await expect(line.getByRole("link", { name: "Its page" })).toHaveAttribute("href", "/agents/tree-s1?own=1");
});

test("the earlier names give way before its own", async ({ page }) => {
  await page.setViewportSize({ width: 400, height: 800 });
  await page.goto("/agents/tree-s1?own=1&task=ts1-b");
  const path = page.getByRole("navigation", { name: "Path" });
  const here = path.locator(".hx-here");
  await expect(here).toHaveText("Measure election time");
  const cut = (l: Locator) => l.evaluate((el) => el.scrollWidth > el.clientWidth);
  expect(await cut(path.getByRole("link", { name: "Leader election over QUIC" }))).toBe(true);
  // Its own keeps its whole name, or 62% of the line when that's less.
  const [shown, whole, line] = await here.evaluate((el) => [el.clientWidth, el.scrollWidth, el.parentElement?.clientWidth ?? 0]);
  expect(shown).toBeGreaterThanOrEqual(Math.min(whole, Math.floor(line * 0.62)) - 1);
});

test("its parent's words carry its name and the way back, and yours say the agent passes them on", async ({ page }) => {
  await page.goto("/agents/relay-lead?task=a575c1d3e5f7a9b1c");
  await expect(names(page)).toHaveText(["Parser rewrite", "Run the parser tests"]);
  const brief = page.locator(".hx-prompt", { hasText: "Run the parser's tests and report what fails." });
  await expect(brief.locator(".hx-by")).toHaveText("Parser rewrite");
  await expect(brief.getByRole("link", { name: "Parser rewrite" })).toHaveAttribute("href", "/agents/relay-lead");
  await expect(page.locator(".hx-prompt .hx-eyebrow", { hasText: /^system$/ })).toHaveCount(0);
  const mine = page.locator(".hx-prompt", { hasText: "Use the v2 parser" }).locator(".hx-prompt-meta");
  await expect(mine.locator(".hx-eyebrow")).toHaveText("You");
  await expect(mine).toContainText("· via Parser rewrite");
});

test("a subagent kiln started: its brief is its parent's", async ({ page }) => {
  await page.goto("/agents/tree-lead?task=tree-s1");
  const brief = page.locator(".hx-prompt").first();
  await expect(brief).toContainText("Step 2 of 5 of the plan");
  await expect(brief.locator(".hx-by")).toHaveText("Storage lead");
  await expect(brief.getByRole("link", { name: "Storage lead" })).toHaveAttribute("href", "/agents/tree-lead");
});

test("a finished subagent's thread ends with Done and the way back to its parent's; one at work doesn't end", async ({ page }) => {
  await page.goto("/agents/tree-s1?own=1&task=ts1-b");
  await expect(page.locator(".hx-prompt .hx-by")).toHaveText("Leader election over QUIC");
  const end = page.locator(".hx-thread-end");
  await expect(end).toHaveAttribute("data-stand", "done");
  await expect(end.locator(".hx-thread-word")).toHaveText("Done");
  await end.getByRole("link", { name: "Leader election over QUIC" }).click();
  await expect(page).toHaveURL(/\/agents\/tree-lead\?task=tree-s1$/);

  await page.goto("/agents/tree-s1?own=1&task=ts1-a");
  await expect(names(page)).toHaveText(["Storage lead", "Leader election over QUIC", "Check vote timeouts"]);
  await expect(page.locator(".hx-thread-end")).toHaveCount(0);
});
