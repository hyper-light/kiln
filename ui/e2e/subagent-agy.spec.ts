/**
 * An Antigravity agent's own subagents are paused and limited as Claude Code's are: kiln's hook in
 * agy's hooks.json before each of their tool calls holds them (docs/agents.md, "Pausing" and "What
 * each may do"). agy's web tools are calls of their own, so its Web choice is whole: Allow, Ask or
 * Off, as each other kind. In the production build against the mock (`agy-kids`, made for these
 * tests).
 */
import { expect, test } from "@playwright/test";

/** Against a real kiln server (playwright.config.ts): these change the mock's agents. */
const REAL = !!process.env.E2E_BASE_URL;

const command = "/api/harness/sessions/agy-kids/command";

test.beforeEach(async ({ request }) => {
  test.skip(REAL, "the mock's agents");
  await request.post("/api/mock/agy-kids");
});

test.afterEach(async ({ request }) => {
  if (!REAL) await request.post("/api/mock/variants");
});

test("an Antigravity subagent pauses at its next tool call, and resumes", async ({ page }) => {
  await page.goto("/agents/agy-kids?task=ag-a");
  const crumbs = page.locator(".hx-crumbs");
  const sent = page.waitForRequest((r) => r.method() === "POST" && r.url().endsWith(command));
  await crumbs.getByRole("button", { name: "Pause Codebase Researcher" }).click();
  expect((await sent).postDataJSON()).toEqual({ type: "pause", on: true, task: "ag-a" });
  const resume = crumbs.getByRole("button", { name: "Resume Codebase Researcher" });
  await expect(resume).toBeEnabled();
  const back = page.waitForRequest((r) => r.method() === "POST" && r.url().endsWith(command));
  await resume.click();
  expect((await back).postDataJSON()).toEqual({ type: "pause", on: false, task: "ag-a" });
});

test("what an Antigravity subagent may do is set as a Claude Code one's, its web included", async ({ page }) => {
  await page.goto("/agents/agy-kids?task=ag-b");
  await page.locator(".hx-crumbs").getByRole("button", { name: "Permissions" }).click();
  const panel = page.getByRole("group", { name: "What Cache Inspector may do" });
  for (const kind of ["Read", "Edit", "Run", "Web"]) {
    for (const g of ["Allow", "Ask", "Off"]) await expect(panel.getByRole("group", { name: kind }).getByRole("button", { name: g })).toBeEnabled();
  }
  for (const field of ["Folder", "Minutes", "Tokens"]) await expect(panel.getByRole("textbox", { name: field })).toBeEnabled();
  await expect(panel).not.toContainText("set for the whole agent");
  const sent = page.waitForRequest((r) => r.method() === "POST" && r.url().endsWith(command));
  // By its key: on a phone the mock's own requests ("Waiting on you") may lie over the last rows.
  await panel.getByRole("group", { name: "Web" }).getByRole("button", { name: "Ask" }).press("Enter");
  expect((await sent).postDataJSON()).toEqual({ type: "perms", task: "ag-b", perms: { read: "allow", edit: "allow", run: "allow", web: "ask" } });
});
