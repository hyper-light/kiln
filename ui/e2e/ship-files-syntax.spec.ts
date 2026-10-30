/**
 * Ship's Files in their language's colours and asked about, as agent chat's diffs are (Ada,
 * 2026-10-29: "the tree-sitter integration for PR code diffs like on our diffs in agent chat ...
 * Including with the agent ask functionality"), against the mock: D41's Rust lines carry
 * tree-sitter's tokens (lib/syntax); D41 is an agent's change, so a name tapped in its code opens
 * the card under that line (components/code/about.tsx), About streams the agent's helper's answer
 * into it, and Close ends it; a line's number still starts a comment. The colours move nothing:
 * every line, and Files itself, measure the same with them and without.
 */
import { expect, test, type Locator, type Page } from "@playwright/test";

const REAL = !!process.env.E2E_BASE_URL;
test.skip(REAL, "drives the mock's commits");

/** D41's Files, opened. */
const filesOf = async (page: Page) => {
  await page.goto("/ship");
  const row = page.locator(".ship-row", { hasText: "pipeline: dynamic matrices" }).first();
  await row.locator(".ship-main").click();
  const files = row.locator('section[aria-label="Files"]');
  await expect(files).toBeVisible();
  return files;
};

/** Where `word` is drawn in a line's code, to tap it there. */
const wordIn = (code: Locator, word: string) =>
  code.evaluate((el, w) => {
    const at = (el.textContent ?? "").indexOf(w);
    const walk = document.createTreeWalker(el, NodeFilter.SHOW_TEXT);
    for (let n = walk.nextNode(), seen = 0; n; seen += n.textContent?.length ?? 0, n = walk.nextNode()) {
      const len = n.textContent?.length ?? 0;
      if (at < 0 || at >= seen + len) continue;
      const range = document.createRange();
      range.setStart(n, at - seen + 1);
      range.setEnd(n, at - seen + 2);
      const r = range.getBoundingClientRect();
      return { x: r.x + r.width / 2, y: r.y + r.height / 2 };
    }
    return null;
  }, word);

test("Files are in their language's colours, as agent chat's diffs are", async ({ page }) => {
  const files = await filesOf(page);
  const line = files.locator(".rv-l", { hasText: "pub fn legs" }).first();
  await expect(line.locator(".rv-t .hl-kw").first()).toHaveText("pub");
  await expect(line.locator(".rv-t .hl-type", { hasText: "Context" })).toHaveCount(1);
  // A removed line is read on its own side.
  await expect(files.locator(".rv-l.d", { hasText: "use serde_json::Value" }).locator(".rv-t .hl-kw").first()).toHaveText("use");
  // TypeScript (a .tsx file) and Markdown in theirs.
  await expect(files.locator(".rv-l", { hasText: "export function Graph" }).first().locator(".rv-t .hl-kw").first()).toHaveText("export");
  await expect(files.locator(".rv-l", { hasText: "## A matrix made by a job" }).first().locator(".rv-t span[class]").first()).toBeVisible();
});

test("in an agent's change, a name tapped asks about it in a card under its line; its number still starts a comment", async ({ page }) => {
  const files = await filesOf(page);
  const line = files.locator(".rv-l", { hasText: "pub fn legs" }).first();
  await line.scrollIntoViewIfNeeded();
  await expect(line.locator(".rv-t[data-ask]")).toHaveCount(1);
  const at = await wordIn(line.locator(".rv-t"), "legs");
  expect(at).not.toBeNull();
  await page.mouse.click(at?.x ?? 0, at?.y ?? 0);
  const card = files.getByRole("region", { name: "About legs" });
  await expect(card).toBeVisible();
  await expect(line).toHaveAttribute("data-asked", "true");
  await expect(card).toContainText("matrix.rs · line 15");
  await card.getByRole("button", { name: "About", exact: true }).click();
  await expect(card).toContainText("What it is", { ignoreCase: true, timeout: 15_000 });
  await card.getByRole("button", { name: "Close" }).click();
  await expect(files.getByRole("region", { name: "About legs" })).toHaveCount(0);
  await expect(line).not.toHaveAttribute("data-asked");
  // The number is a comment's, as it was.
  await line.locator(".rv-ln").click();
  await expect(files.locator(".rv-thread .rvc").first()).toBeVisible();
});

test("the colours move nothing: every line and Files itself measure the same with them and without", async ({ page }) => {
  const files = await filesOf(page);
  await expect(files.locator(".rv-t .hl-kw").first()).toBeVisible();
  await page.evaluate(() => document.fonts.ready.then(() => undefined));
  const measure = () =>
    files.evaluate((el) => ({
      files: Math.round(el.getBoundingClientRect().height * 100) / 100,
      lines: [...el.querySelectorAll<HTMLElement>(".rv-l")].map((l) => Math.round(l.getBoundingClientRect().height * 100) / 100),
    }));
  // Files settled first: two measures a moment apart the same (each file's lines and colours in).
  let coloured = await measure();
  for (let i = 0; i < 40; i++) {
    await page.waitForTimeout(250);
    const again = await measure();
    if (JSON.stringify(again) === JSON.stringify(coloured)) break;
    coloured = again;
  }
  expect(coloured.lines.length).toBeGreaterThan(10);
  // The same lines with their tokens' classes taken off: plain text, as before the grammar came.
  await files.evaluate((el) => el.querySelectorAll(".rv-t span[class]").forEach((s) => s.removeAttribute("class")));
  await expect(files.locator(".rv-t span[class]")).toHaveCount(0);
  expect(await measure()).toEqual(coloured);
});
