/**
 * A subagent's request, answered from its agent's row in Mission control, goes to that subagent,
 * in the production build against the mock (mock/agents.ts, the ask-* agents): Allow goes to it
 * there; Answer and Review open its thread in its agent's chat, its request waiting at the end,
 * and what's answered there goes to it. So does its card on any other page, and its own address
 * (a notification's link): each lands on its thread.
 */
import { expect, test, type Page } from "@playwright/test";

/** Against a real kiln server (playwright.config.ts): these drive the mock's agents. */
const REAL = !!process.env.E2E_BASE_URL;

/** The commands the page sends `agent`'s session over its socket, as it sends them. */
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

/** The shell's cards for what other agents wait on put away, as a person would: on a phone they sit over the conversation's end. */
async function putAwayCards(page: Page) {
  const away = page.getByRole("list", { name: "Waiting on you" }).getByRole("button", { name: /^Put away/ });
  await away
    .first()
    .waitFor({ timeout: 3000 })
    .catch(() => undefined);
  await expect(async () => {
    if ((await away.count()) > 0) await away.first().click({ timeout: 1000 });
    await expect(away).toHaveCount(0, { timeout: 500 });
  }).toPass({ timeout: 10_000 });
}

test.beforeEach(async ({ page }) => {
  test.skip(REAL, "the mock's agents");
  // Each test from the same requests, whichever ran before.
  await page.request.post("/api/mock/asks");
});

test("Answer on a subagent's question opens that subagent's thread, and the answer goes to it", async ({ page }) => {
  const sent = sentOnSocket(page, "ask-s2");
  await page.goto("/agents");
  const row = page.locator('.ms-row[data-agent="ask-lead"]');
  await expect(row.locator(".ms-text")).toHaveText("Its subagent “Step 2: Exec through a seed”: Asks: Which goes first, observe's gate or mine?");
  await row.getByRole("link", { name: "Answer" }).click();
  // Its thread, in its agent's chat, its question at the end.
  await expect(page).toHaveURL(/\/agents\/ask-lead\?task=ask-s2$/);
  await expect(page.locator(".hx-crumbs")).toContainText("Step 2: Exec through a seed");
  await putAwayCards(page);
  await page.getByRole("radio", { name: "Observe's first" }).click();
  await page.getByRole("button", { name: "Submit" }).click();
  await expect.poll(() => sent).toContainEqual(expect.objectContaining({ type: "approve", id: "ask-s2-q1", decision: "allow" }));
});

test("Allow on a subagent's request goes to that subagent, and Review opens its thread", async ({ page }) => {
  await page.goto("/agents");
  const row = page.locator('.ms-row[data-agent="ask-perm"]');
  await expect(row.locator(".ms-text")).toHaveText("Its subagent “Step 1: Check the figures”: Bash: npm run figures -- --check");
  await expect(row.locator("a.ms-link", { hasText: "Review" })).toHaveAttribute("href", "/agents/ask-perm?task=ask-perm-s1");
  const sent = page.waitForRequest((r) => r.method() === "POST" && new URL(r.url()).pathname === "/api/harness/sessions/ask-perm-s1/command");
  await row.getByRole("button", { name: "Allow" }).click();
  expect((await sent).postDataJSON()).toMatchObject({ type: "approve", id: "ask-perm-s1-ap1", decision: "allow" });
  await expect(row).not.toHaveAttribute("data-stand", "needs");
});

test("a subagent's card on another page opens its thread in its agent's chat", async ({ page }) => {
  await page.goto("/runs");
  const card = page.getByRole("list", { name: "Waiting on you" }).getByRole("listitem").filter({ hasText: "Step 2: Exec through a seed" });
  await card.getByRole("button", { name: "Answer" }).click();
  await expect(page).toHaveURL(/\/agents\/ask-lead\?task=ask-s2$/);
  await expect(page.locator(".hx-crumbs")).toContainText("Step 2: Exec through a seed");
  await expect(page.getByRole("radio", { name: "Observe's first" })).toBeVisible();
});

test("a subagent's own address, as its notification links it, opens its thread in its agent's chat", async ({ page }) => {
  await page.goto("/agents/ask-s2");
  await expect(page).toHaveURL(/\/agents\/ask-lead\?task=ask-s2$/);
  await expect(page.locator(".hx-crumbs")).toContainText("Step 2: Exec through a seed");
  await expect(page.getByRole("radio", { name: "Observe's first" })).toBeVisible();
});
