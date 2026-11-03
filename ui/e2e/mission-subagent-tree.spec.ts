/**
 * An agent's subagents under its row in Mission control (subagent-tree.ts), in the production build
 * against the mock (mock/agents.ts, the tree-* agents): its toggle opens and closes the tree; every
 * subagent, kiln's and its harness's own, at any depth, in the order they started, in the row's own
 * columns; the one that needs you has its own piece of the path tan; each opens its thread.
 */
import { expect, test, type Locator } from "@playwright/test";

/** Against a real kiln server (playwright.config.ts): these drive the mock's agents. */
const REAL = !!process.env.E2E_BASE_URL;

const ORDER = ["Leader election over QUIC", "Check vote timeouts", "Measure election time", "Follower catch-up", "Snapshot transfer", "Verify the checksum", "Re-read the manifest", "Docs pass", "Rotate the signing key", "Port the lexer", "Bench the follower"];

const left = async (l: Locator) => Math.round((await l.boundingBox())?.x ?? -1);
/** Where it is on the page (scrolling doesn't move it; layout does). */
const pageTop = (l: Locator) => l.evaluate((el) => el.getBoundingClientRect().top + window.scrollY);

test.beforeEach(async ({ page }) => {
  test.skip(REAL, "the mock's agents");
  // The tree's requests, asked for these tests only (the others' are taken back after).
  await page.request.post("/api/mock/tree-asks");
});

test.afterAll(async ({ request }) => {
  if (!REAL) await request.post("/api/mock/asks");
});

test("the subagents' toggle opens the tree under its row, every subagent in the order it started, and closes it", async ({ page }) => {
  await page.goto("/agents");
  const row = page.locator('.ms-row[data-agent="tree-lead"]');
  const toggle = row.getByRole("button", { name: "Show Storage lead's subagents" });
  await expect(toggle).toHaveAttribute("aria-expanded", "false");
  // One with a subagent of its harness's own has one too; one with none has none.
  await expect(page.locator('.ms-row[data-agent="relay-lead"] .ms-subs')).toHaveCount(1);
  await expect(page.locator('.ms-row[data-agent="fam-tests"] .ms-subs')).toHaveCount(0);
  await expect(page.locator(".ms-tree")).toHaveCount(0);

  await toggle.scrollIntoViewIfNeeded();
  const top = await pageTop(row);
  await toggle.click();
  const tree = page.getByRole("list", { name: "Storage lead's subagents" });
  await expect(tree.locator(".ms-node-name")).toHaveText(ORDER);
  // Opening it moves nothing above it.
  expect(await pageTop(row)).toBe(top);
  await expect(row.getByRole("button", { name: "Hide Storage lead's subagents" })).toHaveAttribute("aria-expanded", "true");

  const stands = await tree.locator(".ms-node").evaluateAll((els) => els.map((e) => e.getAttribute("data-stand")));
  expect(stands).toEqual(["working", "working", "done", "needs", "working", "paused", "working", "done", "needs", "stopped", "failed"]);
  // Those that need you: each its piece of the path tan, and only theirs.
  const catchUp = tree.locator(".ms-node", { hasText: "Follower catch-up" });
  const signing = tree.locator(".ms-node", { hasText: "Rotate the signing key" });
  await expect(catchUp.locator(".ms-node-text")).toHaveText("Bash: kiln run --on linux/amd64 -- cargo test -p hyper-raft catch_up");
  await expect(signing.locator(".ms-node-text")).toHaveText("Bash: gpg --detach-sign dist/kiln-x86_64-linux");
  for (const n of [catchUp, signing]) await expect(n.locator(".tp-hot")).not.toHaveCount(0);
  await expect(tree.locator('.ms-node:not([data-stand="needs"]) .tp-hot')).toHaveCount(0);
  // The row answers its own request, the lead's subagent's: not again here; the other is answered here.
  await expect(row.locator(".ms-text")).toHaveText("Bash: gpg --detach-sign dist/kiln-x86_64-linux");
  await expect(signing.getByRole("button", { name: "Allow" })).toHaveCount(0);
  await expect(catchUp.getByRole("button", { name: "Allow" })).toHaveCount(1);

  await row.getByRole("button", { name: "Hide Storage lead's subagents" }).click();
  await expect(page.locator(".ms-tree")).toHaveCount(0);
});

test("the tree's rows keep to one column: every name at one place, and on a wide screen where the row's own name is", async ({ page, isMobile }) => {
  await page.goto("/agents");
  const row = page.locator('.ms-row[data-agent="tree-lead"]');
  await row.getByRole("button", { name: "Show Storage lead's subagents" }).click();
  const names = page.getByRole("list", { name: "Storage lead's subagents" }).locator(".ms-node-name");
  await expect(names).toHaveCount(ORDER.length);
  const xs = new Set<number>();
  for (const n of await names.all()) xs.add(await left(n));
  expect([...xs]).toHaveLength(1);
  if (!isMobile && (page.viewportSize()?.width ?? 0) > 1000) expect([...xs][0]).toBe(await left(row.locator(".ms-name")));
});

test("Allow on a subagent in the tree goes to it, and it works on", async ({ page }) => {
  await page.goto("/agents");
  await page.locator('.ms-row[data-agent="tree-lead"]').getByRole("button", { name: "Show Storage lead's subagents" }).click();
  const catchUp = page.getByRole("list", { name: "Storage lead's subagents" }).locator(".ms-node", { hasText: "Follower catch-up" });
  await catchUp.getByRole("button", { name: "Allow" }).click();
  await expect(catchUp).toHaveAttribute("data-stand", "working");
  await expect(catchUp.locator(".tp-hot")).toHaveCount(0);
});

test("a subagent in the tree opens its thread on its agent's page", async ({ page }) => {
  await page.goto("/agents");
  await page.locator('.ms-row[data-agent="tree-lead"]').getByRole("button", { name: "Show Storage lead's subagents" }).click();
  const tree = page.getByRole("list", { name: "Storage lead's subagents" });
  await tree.locator(".ms-node", { hasText: "Follower catch-up" }).locator(".ms-node-main").click();
  await expect(page).toHaveURL(/\/agents\/tree-lead\?task=tree-s2$/);
  await page.goto("/agents");
  await page.locator('.ms-row[data-agent="tree-lead"]').getByRole("button", { name: "Show Storage lead's subagents" }).click();
  await page.getByRole("list", { name: "Storage lead's subagents" }).locator(".ms-node", { hasText: "Re-read the manifest" }).locator(".ms-node-main").click();
  await expect(page).toHaveURL(/\/agents\/tree-s3a\?own=1&task=ts3a-a$/);
});
