/**
 * A subagent that waits on you, in its agent's chat, in the production build against the mock
 * (mock/agents.ts, the ask-* agents): the line by the composer says so, and a row over the composer,
 * drawn as Mission control's row that needs you, names it, says what it asks, and answers it as that
 * subagent's. A question opens its thread; Allow goes to it; the row opens its thread, where it has
 * no row of its own.
 */
import { expect, test, type Page } from "@playwright/test";

/** Against a real kiln server (playwright.config.ts): these drive the mock's agents. */
const REAL = !!process.env.E2E_BASE_URL;

/** The shell's cards for what other agents wait on put away, as a person would: on a phone they sit over the composer. */
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

/** The next command posted to `agent`'s session. */
const commandTo = (page: Page, agent: string) => page.waitForRequest((r) => r.method() === "POST" && new URL(r.url()).pathname === `/api/harness/sessions/${agent}/command`);

test.beforeEach(async ({ page }) => {
  test.skip(REAL, "the mock's agents");
  // Each test from the same requests, whichever ran before.
  await page.request.post("/api/mock/asks");
});

test("a subagent kiln started asks you: the line and a row over the composer say so, and Answer opens its thread", async ({ page }) => {
  await page.goto("/agents/ask-lead");
  const line = page.locator(".al-line");
  await expect(line).toContainText("1 subagent");
  await expect(line.locator(".al-needs")).toHaveText("Needs you");
  const row = page.getByRole("group", { name: "Step 2: Exec through a seed needs you" });
  await expect(row.locator(".sa-text")).toHaveText("Asks: Which goes first, observe's gate or mine?");
  await putAwayCards(page);
  await row.getByRole("button", { name: "Answer" }).click();
  await expect(page).toHaveURL(/\/agents\/ask-lead\?task=ask-s2$/);
  await expect(page.locator(".hx-crumbs")).toContainText("Step 2: Exec through a seed");
  await expect(page.getByRole("radio", { name: "Observe's first" })).toBeVisible();
});

test("Allow on a subagent's row goes to that subagent, and then nothing says it needs you", async ({ page }) => {
  await page.goto("/agents/ask-perm");
  const row = page.getByRole("group", { name: "Step 1: Check the figures needs you" });
  await expect(row.locator(".sa-text")).toHaveText("Bash: npm run figures -- --check");
  await putAwayCards(page);
  const sent = commandTo(page, "ask-perm-s1");
  await row.getByRole("button", { name: "Allow" }).click();
  expect((await sent).postDataJSON()).toMatchObject({ type: "approve", id: "ask-perm-s1-ap1", decision: "allow" });
  await expect(row).toHaveCount(0);
  await expect(page.locator(".al-needs")).toHaveCount(0);
});

test("one of its harness's own subagents asks you: its row names it, opens its thread, and Allow goes through its agent", async ({ page }) => {
  await page.goto("/agents/ask-own");
  await expect(page.locator(".al-needs")).toHaveText("Needs you");
  const row = page.getByRole("group", { name: "Check the docs against the code needs you" });
  await expect(row.locator(".sa-text")).toHaveText("Bash: npm run docs -- --check");
  await putAwayCards(page);
  // The row opens its thread, where its request is: no row for it there.
  await row.locator(".sa-main").click();
  await expect(page).toHaveURL(/\/agents\/ask-own\?task=a5d1c3e5f7a9b1c3d$/);
  await expect(row).toHaveCount(0);
  await page.getByRole("button", { name: "Main thread", exact: true }).click();
  await expect(row).toBeVisible();
  const sent = commandTo(page, "ask-own");
  await row.getByRole("button", { name: "Allow" }).click();
  expect((await sent).postDataJSON()).toMatchObject({ type: "approve", id: "ask-own-ap1", decision: "allow" });
  await expect(row).toHaveCount(0);
});
