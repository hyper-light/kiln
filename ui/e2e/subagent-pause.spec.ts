/**
 * Pausing a subagent from wherever it shows: Mission control's tree, the line by the composer, and
 * its thread's crumbs. One kiln started is paused whole (its processes, and what runs in it, with
 * it); one of its harness's own is held at its next tool call. Each says Paused, and its button
 * resumes it. In the production build against the mock (mock/agents.ts, the tree-* agents).
 */
import { expect, test, type Page } from "@playwright/test";

/** Against a real kiln server (playwright.config.ts): these pause the mock's agents. */
const REAL = !!process.env.E2E_BASE_URL;

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

/** The command a session gets over HTTP, as the page sends it. */
const commandTo = (page: Page, session: string) => page.waitForRequest((r) => r.method() === "POST" && r.url().endsWith(`/api/harness/sessions/${session}/command`));

test.beforeEach(() => {
  test.skip(REAL, "the mock's agents");
});

test("Mission control's tree pauses a subagent kiln started, with what runs in it, and resumes it", async ({ page }) => {
  await page.goto("/agents");
  await page.locator('.ms-row[data-agent="tree-lead"]').getByRole("button", { name: "Show Storage lead's subagents" }).click();
  const tree = page.getByRole("list", { name: "Storage lead's subagents" });
  const election = tree.locator(".ms-node", { hasText: "Leader election over QUIC" });
  const votes = tree.locator(".ms-node", { hasText: "Check vote timeouts" });
  await expect(election).toHaveAttribute("data-stand", "working");

  const sent = commandTo(page, "tree-s1");
  await election.getByRole("button", { name: "Pause Leader election over QUIC" }).click();
  expect((await sent).postDataJSON()).toEqual({ type: "pause", on: true });
  await expect(election).toHaveAttribute("data-stand", "paused");
  await expect(election.locator(".ms-word")).toHaveText("Paused");
  // What runs in it is paused with it: said so, and its own pause is the one to end.
  await expect(votes).toHaveAttribute("data-stand", "paused");
  await expect(votes.getByRole("button", { name: "Pause Check vote timeouts" })).toBeDisabled();
  // One that ended stays as it was.
  await expect(tree.locator(".ms-node", { hasText: "Measure election time" })).toHaveAttribute("data-stand", "done");

  await election.getByRole("button", { name: "Resume Leader election over QUIC" }).click();
  await expect(election).toHaveAttribute("data-stand", "working");
  await expect(votes).toHaveAttribute("data-stand", "working");
  await expect(election.getByRole("button", { name: "Pause Leader election over QUIC" })).toBeEnabled();
});

test("the line by the composer holds one of the agent's own subagents at its next tool call, and resumes it", async ({ page }) => {
  await page.goto("/agents/tree-lead");
  await putAwayCards(page);
  await page.locator(".al-line").click();
  const row = page.locator(".dn", { hasText: "Rotate the signing key" });
  await expect(row).toHaveAttribute("data-stand", "working");
  const sent = commandTo(page, "tree-lead");
  await row.getByRole("button", { name: "Pause Rotate the signing key" }).click();
  expect((await sent).postDataJSON()).toEqual({ type: "pause", on: true, task: "tl-h0" });
  await expect(row).toHaveAttribute("data-stand", "paused");
  await row.getByRole("button", { name: "Resume Rotate the signing key" }).click();
  await expect(row).toHaveAttribute("data-stand", "working");
});

test("a subagent's thread pauses it from its crumbs, and one that ended can't be", async ({ page }) => {
  await page.goto("/agents/tree-s1?own=1&task=ts1-a");
  const line = page.locator(".hx-crumbs");
  await line.getByRole("button", { name: "Pause Check vote timeouts" }).click();
  await expect(line.getByRole("button", { name: "Resume Check vote timeouts" })).toBeEnabled();
  await line.getByRole("button", { name: "Resume Check vote timeouts" }).click();
  await expect(line.getByRole("button", { name: "Pause Check vote timeouts" })).toBeEnabled();

  await page.goto("/agents/tree-s1?own=1&task=ts1-b");
  await expect(page.locator(".hx-crumbs").getByRole("button", { name: "Pause Measure election time" })).toBeDisabled();
});
