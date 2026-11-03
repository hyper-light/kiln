/**
 * An agent's subagents by its composer (background.tsx's line, subagent-tree.ts), in the production
 * build against the mock (mock/agents.ts, the tree-* agents): the line opens to the tree with pick
 * boxes, the select-all where its mark was; those picked are on the composer's To line, and what's
 * sent goes to each its own way (the agent's own subagent over its socket, kiln's over HTTP), each
 * name marked delivered or waiting, and nothing in the agent's own chat.
 */
import { expect, test, type Page } from "@playwright/test";

/** Against a real kiln server (playwright.config.ts): these drive the mock's agents. */
const REAL = !!process.env.E2E_BASE_URL;

const ORDER = ["Leader election over QUIC", "Check vote timeouts", "Measure election time", "Follower catch-up", "Snapshot transfer", "Verify the checksum", "Re-read the manifest", "Docs pass", "Rotate the signing key", "Port the lexer", "Bench the follower"];

/** The commands the page sends `agent`'s session over its socket. */
function sentOnSocket(page: Page, agent: string): Record<string, unknown>[] {
  const sent: Record<string, unknown>[] = [];
  page.on("websocket", (ws) => {
    if (!ws.url().includes(`/api/harness/sessions/${agent}/`)) return;
    ws.on("framesent", (f) => {
      try {
        const v = JSON.parse(String(f.payload)) as { c?: Record<string, unknown> } & Record<string, unknown>;
        sent.push(v.c ?? v);
      } catch {
        // Not a command.
      }
    });
  });
  return sent;
}

/** The shell's cards for what other agents wait on put away: on a phone they sit over the composer. */
async function putAwayCards(page: Page) {
  const away = page.getByRole("list", { name: "Waiting on you" }).getByRole("button", { name: /^Put away/ });
  await away
    .first()
    .waitFor({ timeout: 3000 })
    .catch(() => undefined);
  await expect(async () => {
    if ((await away.count()) > 0) await away.first().click({ timeout: 1000 });
    await expect(away).toHaveCount(0, { timeout: 500 });
  }).toPass({ timeout: 15_000 });
}

test.beforeEach(async ({ page }) => {
  test.skip(REAL, "the mock's agents");
  await page.request.post("/api/mock/tree-asks");
});

test.afterAll(async ({ request }) => {
  if (!REAL) await request.post("/api/mock/asks");
});

test("the line opens to its subagents' tree, a pick box each, every one picked at once where its mark was", async ({ page }) => {
  await page.goto("/agents/tree-lead");
  await putAwayCards(page);
  const line = page.locator(".al");
  await expect(line.locator(".al-slot .kg")).toHaveCount(1);
  await line.locator(".al-line").click();
  const rows = line.locator(".dn");
  await expect(rows.locator(".dn-name")).toHaveText(ORDER);
  const all = page.getByRole("checkbox", { name: "Pick every subagent" });
  await all.check();
  // Every one: those that take a message, and those that ended (picked to be removed).
  await expect(line.locator(".dn[data-picked]")).toHaveCount(ORDER.length);
  await expect(page.getByRole("checkbox", { name: "Pick Measure election time" })).toBeChecked();
  await all.uncheck();
  await expect(line.locator(".dn[data-picked]")).toHaveCount(0);
});

test("those picked are on the To line; one message goes to each its own way, each marked, none to the agent", async ({ page }) => {
  // A phone's To line names two, then counts the rest.
  const narrow = (page.viewportSize()?.width ?? 0) < 720;
  const socket = sentOnSocket(page, "tree-lead");
  await page.goto("/agents/tree-lead");
  await putAwayCards(page);
  const line = page.locator(".al");
  await line.locator(".al-line").click();
  for (const name of ["Leader election over QUIC", "Rotate the signing key", "Verify the checksum"]) await page.getByRole("checkbox", { name: `Pick ${name}` }).check();
  const to = page.getByRole("group", { name: "To" });
  await expect(to).toBeVisible();
  const http = page.waitForRequest((r) => r.url().includes("/api/harness/sessions/tree-s1/command") && r.method() === "POST");
  const field = page.getByRole("textbox").last();
  await field.fill("Use 64 KiB chunks.");
  // Send, as a phone has it (Enter there starts a new line).
  await page.locator(".hx-dock").getByRole("button", { name: "Send", exact: true }).click();
  // The lead's own harness's subagent over its socket, with its task; kiln's over HTTP to its session.
  expect((await http).postDataJSON()).toMatchObject({ type: "send", text: "Use 64 KiB chunks." });
  await expect.poll(() => socket.some((c) => c.type === "send" && c.task === "tl-h0" && c.text === "Use 64 KiB chunks.")).toBe(true);
  // Delivered to the one at work; queued for the one waiting on its request, and for the one paused at its limit.
  await expect(to.locator('.hx-to-name[data-mark="ok"]')).toHaveText(["Leader election over QUIC"], { timeout: 10_000 });
  await expect(to.locator('.hx-to-name[data-mark="wait"]')).toHaveText(narrow ? ["Verify the checksum"] : ["Verify the checksum", "Rotate the signing key"]);
  if (narrow) await expect(to.locator(".hx-to-more")).toHaveText("+1");
  // Nothing went to the agent itself.
  await expect(page.locator(".hx-prompt", { hasText: "Use 64 KiB chunks." })).toHaveCount(0);
  await to.getByRole("button", { name: "Clear who it's to" }).click();
  await expect(page.getByRole("group", { name: "To" })).toHaveCount(0);
});

test("Allow on a subagent in the tree by the composer goes to it, and it works on", async ({ page }) => {
  await page.goto("/agents/tree-lead");
  await putAwayCards(page);
  const line = page.locator(".al");
  await line.locator(".al-line").click();
  const catchUp = line.locator(".dn", { hasText: "Follower catch-up" });
  await expect(catchUp).toHaveAttribute("data-stand", "needs");
  await catchUp.getByRole("button", { name: "Allow" }).click();
  await expect(catchUp).toHaveAttribute("data-stand", "working");
});
