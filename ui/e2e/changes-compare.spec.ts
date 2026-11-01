/**
 * The Changes tab's Compare (compare.tsx; the Changes panel plan 01a194444b1d, step 5), on the
 * mock's fam-orch: From its trunk To Now at first; each picked in place from a list that filters
 * as you type, of this session's points (Now, Before it began, each agent's version) and its
 * repository's branches, tags and commits. Two subagents compare plainly, each file naming who
 * changed it; Swap turns the diff around. "Only what To adds since they split" and Swap sit in the
 * head's menu. Where its machine can't say what it has, Compare is off and says why.
 */
import { expect, test, type Page } from "@playwright/test";

/** Against a real kiln server (playwright.config.ts): these drive the mock's agents. */
const REAL = !!process.env.E2E_BASE_URL;

async function open(page: Page, agent = "fam-orch") {
  await page.goto(`/agents/${agent}?tab=changes`);
  const tab = page
    .getByRole("tab", { name: /^Changes/ })
    .filter({ visible: true })
    .first();
  await tab.click();
  await expect(page.locator(".cx-head")).toBeVisible();
}

async function compareMode(page: Page) {
  const mode = page.getByRole("radio", { name: "Compare" });
  await expect(mode).toBeEnabled();
  await mode.click();
  await expect(page.getByRole("button", { name: /^To:/ })).toBeVisible();
}

async function point(page: Page, which: "From" | "To", name: RegExp, filter?: string) {
  await page.getByRole("button", { name: new RegExp(`^${which}:`) }).click();
  const list = page.locator(".cx-pick");
  await expect(list).toBeVisible();
  if (filter) await list.locator(".cx-filter").fill(filter);
  await list.getByRole("button", { name }).click();
  await expect(list).toBeHidden();
}

test("From its trunk To Now at first; the split option and Swap sit in the head's menu", async ({ page }) => {
  test.skip(REAL, "the mock's agents");
  await open(page);
  await compareMode(page);
  await expect(page.getByRole("button", { name: "From: origin/main" })).toBeVisible();
  await expect(page.getByRole("button", { name: "To: Now" })).toBeVisible();
  await expect(page.locator(".cx-file:visible")).not.toHaveCount(0);
  await page.getByRole("button", { name: /^Sort:/ }).click();
  const only = page.getByRole("menuitemcheckbox", { name: /^Only what To adds since they split/ });
  await expect(only).toHaveAttribute("aria-checked", "true");
  await only.click();
  await page.getByRole("button", { name: /^Sort:/ }).click();
  await expect(page.getByRole("menuitemcheckbox", { name: /^Only what To adds since they split/ })).toHaveAttribute("aria-checked", "false");
  await page.keyboard.press("Escape");
  // The list filters as you type, and says when nothing's called that.
  await page.getByRole("button", { name: /^From:/ }).click();
  await page.locator(".cx-filter").fill("v0.1");
  await expect(page.locator(".cx-pick .cx-ref")).toHaveText(["v0.1.9"]);
  await page.locator(".cx-filter").fill("no such point");
  await expect(page.locator(".cx-pick-none")).toHaveText("Nothing called that.");
});

test("two subagents compare plainly, each file naming who changed it; Swap turns the diff around", async ({ page }) => {
  test.skip(REAL, "the mock's agents");
  await open(page);
  await compareMode(page);
  await point(page, "From", /^Plan the planner goal/);
  await point(page, "To", /^Plan the API optimization goal/, "optimization");
  const plans = page.locator(".cx-file", { hasText: "plans.md" });
  // What only the planner added isn't in the optimizer's version: deleted, from one to the other.
  await expect(plans.locator(".hx-tag")).toHaveText("deleted");
  await expect(plans.locator(".cx-by")).toHaveText("changed by Plan the planner goal");
  await expect(page.locator(".cx-file", { hasText: "quic.rs" }).locator(".cx-by")).toHaveText("changed by Plan the API optimization goal");
  await page.getByRole("button", { name: /^Sort:/ }).click();
  await page.getByRole("menuitem", { name: "Swap From and To" }).click();
  await expect(page.getByRole("button", { name: /^From: Plan the API optimization goal/ })).toBeVisible();
  await expect(plans.locator(".hx-tag")).toHaveText("new");
});

test("where its machine can't say what it has, Compare is off and says why", async ({ page }) => {
  test.skip(REAL, "the mock's agents");
  await open(page, "fam-tests");
  const mode = page.getByRole("radio", { name: "Compare" });
  await expect(mode).toBeDisabled();
  await expect(mode).toHaveAttribute("title", /isn't in a git repository/);
});
