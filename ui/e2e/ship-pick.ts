/**
 * Ship's rows are repositories (docs/ci.md W26-R29): an opened one's first section is its commits,
 * and the sections under them are the picked commit's. The specs reach a commit through here.
 */
import { expect, type Locator, type Page } from "@playwright/test";

/** `repo`'s row (kiln's by default). */
export function repoRow(page: Page, repo = "hyper-light/kiln"): Locator {
  return page
    .getByRole("list", { name: "Repositories", exact: true })
    .locator(":scope > li")
    .filter({ has: page.locator(".ship-name", { hasText: repo }) })
    .first();
}

/**
 * `repo`'s row, opened, with the commit whose line says `text` picked (paging through its commits
 * to find it), so the row's sections are that commit's.
 */
export async function pickCommit(page: Page, text: string | RegExp, repo = "hyper-light/kiln"): Promise<Locator> {
  const row = repoRow(page, repo);
  // The row drawn first (a first load on a busy machine takes a while), then opened unless it is.
  await expect(row).toBeVisible({ timeout: 15_000 });
  const go = row.locator(".ship-go");
  if ((await go.getAttribute("aria-expanded")) !== "true") await go.click();
  const commits = row.locator("section.ship-commits");
  await expect(commits.locator(".sc-c").first()).toBeVisible({ timeout: 15_000 });
  const line = commits.locator(".sc-c", { hasText: text }).first();
  for (let i = 0; i < 20 && (await line.count()) === 0; i++) {
    const older = commits.getByRole("button", { name: "Older", exact: true });
    if (await older.isDisabled()) break;
    await older.click();
  }
  await line.getByRole("button").click();
  await expect(line.getByRole("button")).toHaveAttribute("aria-pressed", "true");
  return row;
}
