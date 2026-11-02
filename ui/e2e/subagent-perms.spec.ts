/**
 * What a person lets a subagent do, from its thread: the shield on its crumbs opens the panel under
 * them (no popup); each change goes at once, a choice wider than what it's under is off, and every
 * choice sits at the panel's right edge. In the production build against the mock (mock/agents.ts:
 * tree-s1 lets its subagents run commands only asking).
 */
import { expect, test, type Locator } from "@playwright/test";

/** Against a real kiln server (playwright.config.ts): these change the mock's agents. */
const REAL = !!process.env.E2E_BASE_URL;

const right = async (l: Locator) => {
  const b = await l.boundingBox();
  return b ? Math.round(b.x + b.width) : -1;
};

test.beforeEach(() => {
  test.skip(REAL, "the mock's agents");
});

test.afterEach(async ({ request }) => {
  // What the tests set is taken back, for the next.
  if (!REAL) await request.post("/api/harness/sessions/tree-s1/command", { data: { type: "perms", task: "ts1-a" } });
});

test("the shield opens what a subagent may do under its crumbs; a change goes at once, and a choice wider than its parent's is off", async ({ page }) => {
  await page.goto("/agents/tree-s1?own=1&task=ts1-a");
  const shield = page.locator(".hx-crumbs").getByRole("button", { name: "Permissions" });
  await expect(shield).toHaveAttribute("aria-expanded", "false");
  await shield.click();
  await expect(shield).toHaveAttribute("aria-expanded", "true");
  const panel = page.getByRole("group", { name: "What Check vote timeouts may do" });
  // Under Leader election over QUIC, which runs commands only asking: Run's Allow isn't offered.
  const run = panel.getByRole("group", { name: "Run" });
  await expect(run.getByRole("button", { name: "Allow" })).toBeDisabled();
  await expect(run.getByRole("button", { name: "Ask" })).toHaveAttribute("aria-pressed", "true");

  const edit = panel.getByRole("group", { name: "Edit" });
  const sent = page.waitForRequest((r) => r.method() === "POST" && r.url().endsWith("/api/harness/sessions/tree-s1/command"));
  await edit.getByRole("button", { name: "Off" }).click();
  expect((await sent).postDataJSON()).toEqual({ type: "perms", task: "ts1-a", perms: { read: "allow", edit: "off", run: "ask", web: "allow" } });
  await expect(edit.getByRole("button", { name: "Off" })).toHaveAttribute("aria-pressed", "true");

  // A limit, set when it's left.
  const minutes = panel.getByRole("textbox", { name: "Minutes" });
  const limited = page.waitForRequest((r) => r.method() === "POST" && r.url().endsWith("/api/harness/sessions/tree-s1/command"));
  await minutes.fill("20");
  await minutes.press("Enter");
  expect((await limited).postDataJSON()).toMatchObject({ type: "perms", task: "ts1-a", perms: { edit: "off", minutes: 20 } });

  await shield.click();
  await expect(panel).toHaveCount(0);
});

test("every choice in the panel sits at its right edge", async ({ page }) => {
  await page.goto("/agents/tree-s1?own=1&task=ts1-a");
  await page.locator(".hx-crumbs").getByRole("button", { name: "Permissions" }).click();
  const panel = page.locator(".hx-perm");
  const edges = new Set<number>();
  for (const l of await panel.locator(".hx-perm-seg, .hx-perm-where, .hx-perm-limits").all()) edges.add(await right(l));
  expect([...edges]).toHaveLength(1);
  const pad = await panel.evaluate((el) => parseFloat(getComputedStyle(el).paddingRight));
  expect([...edges][0]).toBe((await right(panel)) - Math.round(pad));
});

test("one that ended can't be changed", async ({ page }) => {
  await page.goto("/agents/tree-s1?own=1&task=ts1-b");
  await page.locator(".hx-crumbs").getByRole("button", { name: "Permissions" }).click();
  const panel = page.getByRole("group", { name: "What Measure election time may do" });
  await expect(panel.getByRole("group", { name: "Edit" }).getByRole("button", { name: "Off" })).toBeDisabled();
  await expect(panel.getByRole("textbox", { name: "Folder" })).toBeDisabled();
});
