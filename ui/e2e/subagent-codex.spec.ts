/**
 * A Codex agent's own subagents are paused and limited as Claude Code's are: kiln's hook before
 * each of their tool calls holds them (docs/agents.md, "Pausing" and "What each may do"). Codex sets
 * its web search for the whole agent alone, so a subagent of its own has no Web choice, and the
 * agent no Ask: the panel says so. In the production build against the mock (`codex-kids`, made for
 * these tests; tree-s2, a Codex session kiln started under Storage lead).
 */
import { expect, test } from "@playwright/test";

/** Against a real kiln server (playwright.config.ts): these change the mock's agents. */
const REAL = !!process.env.E2E_BASE_URL;

test.beforeEach(async ({ request }) => {
  test.skip(REAL, "the mock's agents");
  await request.post("/api/mock/codex-kids");
});

test.afterEach(async ({ request }) => {
  if (!REAL) await request.post("/api/mock/variants");
});

test("a Codex subagent pauses at its next tool call, and resumes", async ({ page }) => {
  await page.goto("/agents/codex-kids?task=ck-a");
  const crumbs = page.locator(".hx-crumbs");
  const sent = page.waitForRequest((r) => r.method() === "POST" && r.url().endsWith("/api/harness/sessions/codex-kids/command"));
  await crumbs.getByRole("button", { name: "Pause Port the lexer" }).click();
  expect((await sent).postDataJSON()).toEqual({ type: "pause", on: true, task: "ck-a" });
  const resume = crumbs.getByRole("button", { name: "Resume Port the lexer" });
  await expect(resume).toBeEnabled();
  const back = page.waitForRequest((r) => r.method() === "POST" && r.url().endsWith("/api/harness/sessions/codex-kids/command"));
  await resume.click();
  expect((await back).postDataJSON()).toEqual({ type: "pause", on: false, task: "ck-a" });
});

test("what a Codex subagent may do is set as a Claude Code one's, but its web search is the whole agent's", async ({ page }) => {
  await page.goto("/agents/codex-kids?task=ck-b");
  await page.locator(".hx-crumbs").getByRole("button", { name: "Permissions" }).click();
  const panel = page.getByRole("group", { name: "What Port the parser tests may do" });
  const web = panel.getByRole("group", { name: "Web" });
  for (const g of ["Allow", "Ask", "Off"]) await expect(web.getByRole("button", { name: g })).toBeDisabled();
  await expect(panel).toContainText("Web search is set for the whole agent.");
  const sent = page.waitForRequest((r) => r.method() === "POST" && r.url().endsWith("/api/harness/sessions/codex-kids/command"));
  await panel.getByRole("group", { name: "Edit" }).getByRole("button", { name: "Off" }).click();
  expect((await sent).postDataJSON()).toEqual({ type: "perms", task: "ck-b", perms: { read: "allow", edit: "off", run: "allow", web: "allow" } });
});

test("a Codex session's web search is allowed or off, never asked", async ({ page }) => {
  await page.goto("/agents/tree-lead?task=tree-s2");
  await page.locator(".hx-crumbs").getByRole("button", { name: "Permissions" }).click();
  const panel = page.getByRole("group", { name: "What Follower catch-up may do" });
  const web = panel.getByRole("group", { name: "Web" });
  await expect(web.getByRole("button", { name: "Ask" })).toBeDisabled();
  await expect(web.getByRole("button", { name: "Off" })).toBeEnabled();
  await expect(panel).toContainText("It can't ask before a web search.");
});
