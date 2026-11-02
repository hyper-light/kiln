/**
 * Branching a subagent from its thread: what changes, then a Variant beside it (under the same
 * agent, marked as one in the tree) or a New agent of its own; and Keep on a finished variant,
 * which tells its parent to use its result. In the production build against the mock
 * (mock/agents.ts: the tree-* agents, `POST /api/mock/tree-variant`).
 */
import { expect, test, type Page } from "@playwright/test";

/** Against a real kiln server (playwright.config.ts): these branch the mock's agents. */
const REAL = !!process.env.E2E_BASE_URL;

/** The shell's cards for what other agents wait on put away: on a phone they sit over the thread's end. */
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

/** The command a session gets over HTTP, as the page sends it. */
const commandTo = (page: Page, session: string) => page.waitForRequest((r) => r.method() === "POST" && r.url().endsWith(`/api/harness/sessions/${session}/command`));

test.beforeEach(() => {
  test.skip(REAL, "the mock's agents");
});

test.afterEach(async ({ request }) => {
  // The variants made here are taken back, for the next tests.
  if (!REAL) await request.post("/api/mock/variants");
});

test("Branch makes a Variant beside the subagent, marked in the tree; New agent makes one of its own", async ({ page }) => {
  await page.goto("/agents/tree-lead?task=tree-s1");
  const fork = page.locator(".hx-crumbs").getByRole("button", { name: "Branch" });
  await fork.click();
  await expect(fork).toHaveAttribute("aria-expanded", "true");
  const panel = page.getByRole("group", { name: "Branch Leader election over QUIC" });
  await expect(panel.getByRole("button", { name: "Variant" })).toBeDisabled();
  await expect(panel.getByRole("button", { name: "New agent" })).toBeDisabled();
  await panel.getByRole("textbox", { name: "What changes" }).fill("Batch entries 64 at a time");
  const sent = commandTo(page, "tree-s1");
  await panel.getByRole("button", { name: "Variant" }).click();
  expect((await sent).postDataJSON()).toEqual({ type: "branch", words: "Batch entries 64 at a time", top: false });
  await expect(panel).toHaveCount(0);

  // Beside it, under the same agent, the fork before its name.
  await page.goto("/agents");
  await page.locator('.ms-row[data-agent="tree-lead"]').getByRole("button", { name: "Show Storage lead's subagents" }).click();
  const tree = page.getByRole("list", { name: "Storage lead's subagents" });
  const variant = tree.locator(".ms-node", { hasText: "Batch entries 64 at a time" });
  await expect(variant.getByRole("img", { name: "Variant" })).toHaveCount(1);
  await expect(variant).toHaveAttribute("data-depth", (await tree.locator(".ms-node", { hasText: "Leader election over QUIC" }).getAttribute("data-depth")) ?? "");

  await page.goto("/agents/tree-lead?task=tree-s1");
  await page.locator(".hx-crumbs").getByRole("button", { name: "Branch" }).click();
  await page.getByRole("textbox", { name: "What changes" }).fill("Elect over TCP instead");
  const top = commandTo(page, "tree-s1");
  await page.getByRole("button", { name: "New agent" }).click();
  expect((await top).postDataJSON()).toEqual({ type: "branch", words: "Elect over TCP instead", top: true });
});

test("Keep on a finished variant tells its parent to use its result; one at work has none", async ({ page, request }) => {
  await request.post("/api/mock/tree-variant");
  await page.goto("/agents/tree-lead?task=tree-s2v");
  await putAwayCards(page);
  const end = page.locator(".hx-thread-end");
  await expect(end).toHaveAttribute("data-stand", "done");
  const sent = commandTo(page, "tree-lead");
  await end.getByRole("button", { name: "Keep" }).click();
  const body = (await sent).postDataJSON() as { type: string; mode: string; text: string };
  expect(body).toMatchObject({ type: "send", mode: "queue" });
  expect(body.text).toContain("“Catch up in 64-entry batches”, the variant of “Follower catch-up”");
  expect(body.text).toContain("agents_result (tree-s2v)");
  await expect(end.getByRole("button", { name: "Kept" })).toBeDisabled();

  await page.goto("/agents/tree-lead?task=tree-s1");
  await expect(page.getByRole("button", { name: "Keep" })).toHaveCount(0);
});
