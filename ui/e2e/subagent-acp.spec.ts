/**
 * An ACP agent's subagents, where it reports them (opencode's child sessions), are paused and
 * limited as Claude Code's and Codex's are, but kiln holds an ACP agent's calls only at those it
 * asks about (`session/request_permission`): the panel says so (docs/agents.md, "Pausing" and
 * "What each may do"). In the production build against the mock (`acp-kids`, made for these tests).
 */
import { expect, test } from "@playwright/test";

/** Against a real kiln server (playwright.config.ts): these change the mock's agents. */
const REAL = !!process.env.E2E_BASE_URL;

test.beforeEach(async ({ request }) => {
  test.skip(REAL, "the mock's agents");
  await request.post("/api/mock/acp-kids");
});

test.afterEach(async ({ request }) => {
  if (!REAL) await request.post("/api/mock/variants");
});

const command = (r: { method(): string; url(): string }) => r.method() === "POST" && r.url().endsWith("/api/harness/sessions/acp-kids/command");

test("an opencode subagent pauses at the next call it asks about, and resumes", async ({ page }) => {
  await page.goto("/agents/acp-kids?task=ses_ak1");
  const crumbs = page.locator(".hx-crumbs");
  const sent = page.waitForRequest(command);
  await crumbs.getByRole("button", { name: "Pause Explore the lexer" }).click();
  expect((await sent).postDataJSON()).toEqual({ type: "pause", on: true, task: "ses_ak1" });
  const resume = crumbs.getByRole("button", { name: "Resume Explore the lexer" });
  await expect(resume).toBeEnabled();
  const back = page.waitForRequest(command);
  await resume.click();
  expect((await back).postDataJSON()).toEqual({ type: "pause", on: false, task: "ses_ak1" });
});

test("what an opencode subagent may do is set as any other's, and the panel says when it holds", async ({ page }) => {
  await page.goto("/agents/acp-kids?task=ses_ak2");
  await page.locator(".hx-crumbs").getByRole("button", { name: "Permissions" }).click();
  const panel = page.getByRole("group", { name: "What Write unicode tests may do" });
  await expect(panel).toContainText("Held when it asks before a call; what its own rules let it do without asking goes as they say.");
  for (const k of ["Read", "Edit", "Run", "Web"]) await expect(panel.getByRole("group", { name: k }).getByRole("button", { name: "Ask" })).toBeEnabled();
  const sent = page.waitForRequest(command);
  await panel.getByRole("group", { name: "Run" }).getByRole("button", { name: "Off" }).click();
  expect((await sent).postDataJSON()).toEqual({ type: "perms", task: "ses_ak2", perms: { read: "allow", edit: "allow", run: "off", web: "allow" } });
});

test("a Codex subagent's panel doesn't carry the ACP line", async ({ page, request }) => {
  await request.post("/api/mock/codex-kids");
  await page.goto("/agents/codex-kids?task=ck-b");
  await page.locator(".hx-crumbs").getByRole("button", { name: "Permissions" }).click();
  await expect(page.getByRole("group", { name: "What Port the parser tests may do" })).not.toContainText("Held when it asks");
});
