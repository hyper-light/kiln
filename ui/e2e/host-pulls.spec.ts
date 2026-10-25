/**
 * Pull requests on a repository's host (docs/ci.md W23-R06, AC03), in the production build against
 * the mock's GitLab merge requests: the inbox lists the ones asking your review; one's page shows
 * its checks, conversation and files; a comment on a line and a reply go out at once (no drafts on
 * a host); an approval shows among its reviews; and it merges by a method the host allows. The
 * desktop merges !3 by squash, the phone !4 by merge commit.
 */
import { expect, test } from "@playwright/test";

/** Against a real kiln server (playwright.config.ts): these drive the mock's merge requests. */
const REAL = !!process.env.E2E_BASE_URL;

test.skip(REAL, "drives the mock's merge requests");

test("a host's merge request: reviewed, commented on line by line, and merged from kiln", async ({ page }, info) => {
  const phone = info.project.name === "phone";
  const n = phone ? 4 : 3;
  const method = phone ? "merge" : "squash";

  await page.goto("/review");
  const row = page.getByRole("link", { name: new RegExp(`!${n} .*lundhe/w23 on GitLab`) });
  await expect(row).toBeVisible();
  await row.click();
  await expect(page).toHaveURL(new RegExp(`/review/hosts/lundhe%2Fw23/${n}$`));
  await expect(page.getByRole("heading", { level: 1 })).toBeVisible();
  await expect(page.getByLabel("Checks on its head commit")).toContainText(`pipeline #${n * 4}`);
  await expect(page.getByText("This is the line that fails.")).toBeVisible();
  // A host's files aren't ticked off as viewed here.
  await expect(page.getByText("Viewed", { exact: true })).toHaveCount(0);

  // A comment on a line goes out at once: one button, no draft.
  await page.getByRole("button", { name: "Comment on line 7" }).first().click();
  const fresh = page.locator(".rv-new");
  await expect(fresh.getByRole("button", { name: "Add to your review" })).toHaveCount(0);
  await fresh.getByPlaceholder("Say what should change, or ask").fill("Why always?");
  await fresh.getByRole("button", { name: "Comment", exact: true }).click();
  await expect(page.getByText("Why always?")).toBeVisible();

  // A reply to the thread already there.
  await page.locator(".rv-thread", { hasText: "This is the line that fails." }).getByRole("button", { name: "Reply" }).click();
  await page.getByPlaceholder("Reply").fill("Because the job fails.");
  await page.getByPlaceholder("Reply").press("Control+Enter");
  await expect(page.getByText("Because the job fails.")).toBeVisible();

  // An approval.
  await page.locator(".rv-review select").selectOption("approve");
  await page.getByRole("button", { name: "Send review" }).click();
  await expect(page.getByLabel("Its reviews")).toContainText("approved");

  // Merged by the method picked.
  await page.locator(".rv-land select").selectOption(method);
  await page.getByRole("button", { name: `Merge !${n}` }).click();
  await expect(page.locator(".rv-state")).toHaveText(/merged/i);
  await expect(page.locator(".rv-land")).toHaveCount(0);

  // Nothing scrolls sideways.
  const wide = await page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);
  expect(wide).toBeLessThanOrEqual(0);
});
