/**
 * Ship's agent fixes (docs/ci.md W26-R18, W26 decision 5): the Conversation head shows the
 * repository's setting read-only, on or off with who set it and when, and links to the repository's
 * Review section in Repos, where it's set. Off, on, or unreadable, it's the same box in the same
 * place, so nothing moves. The repository's setting is put back off after.
 */
import { expect, test, type BrowserContext, type Locator, type Page } from "@playwright/test";
import { pickCommit } from "./ship-pick";

const REAL = !!process.env.E2E_BASE_URL;
test.skip(REAL, "drives the mock's commits");

const REPO = "hyper-light/kiln";
const FIXES = `/api/repos/${encodeURIComponent(REPO)}/agent-fixes`;

/** D41's Conversation, picked among its repository's commits (W26-R29), in a page of its own. */
async function conversation(context: BrowserContext): Promise<{ page: Page; convo: Locator }> {
  const page = await context.newPage();
  await page.goto("/ship");
  const row = await pickCommit(page, "pipeline: dynamic matrices", REPO);
  const convo = row.getByRole("region", { name: "Conversation" });
  await convo.locator(".rv-switch").waitFor();
  return { page, convo };
}

/** The head, the control and what comes under them, each within the section, to the pixel. */
async function boxes(convo: Locator) {
  return convo.evaluate((s) => {
    const at = s.getBoundingClientRect();
    return [...s.children].slice(0, 3).map((e) => {
      const r = e.getBoundingClientRect();
      return [r.left - at.left, r.top - at.top, r.width, r.height].map(Math.round);
    });
  });
}

const SET_DAY = String.raw`\d{1,2} [A-Z][a-z]{2,3} \d{4}`;

test("w26_r18 the Conversation head reads the repository's agent fixes, who set them and when, and links to where they're set; nothing moves", async ({ context, request }) => {
  expect((await request.put(FIXES, { data: { agents_fix: false } })).status()).toBe(200);
  try {
    const first = await conversation(context);
    const off = first.convo.getByRole("link", { name: /^agent fixes: off\./ });
    await expect(off).toHaveAttribute("title", new RegExp(`^agent fixes: off\\. Turned off by the owner, ${SET_DAY}\\. It's set in the repository's Review section in Repos\\.$`));
    await expect(off).toHaveAttribute("href", "/repos?repo=hyper-light%2Fkiln&section=review");
    await expect(off).not.toHaveAttribute("data-on", "");
    const before = await boxes(first.convo);

    // Turned on where it's set, Ship reads it on, in the same box.
    expect((await request.put(FIXES, { data: { agents_fix: true } })).status()).toBe(200);
    const { page, convo } = await conversation(context);
    const on = convo.getByRole("link", { name: new RegExp(`^agent fixes: on\\. Turned on by the owner, ${SET_DAY}\\.`) });
    await expect(on).toHaveAttribute("data-on", "");
    expect(await boxes(convo)).toEqual(before);

    // It links to the repository's Review section in Repos, opened and in view.
    await on.click();
    await expect(page).toHaveURL(/\/repos\?repo=hyper-light%2Fkiln&section=review$/);
    const review = page.getByRole("region", { name: "Review" });
    await expect(review.getByRole("switch", { name: "Agents fix from agents' feedback" })).toHaveAttribute("aria-checked", "true");
    await expect(review).toBeInViewport();
  } finally {
    await request.put(FIXES, { data: { agents_fix: false } });
  }
});

test("w26_r18 a setting Ship can't read keeps the control's box, saying why", async ({ context }) => {
  const before = await boxes((await conversation(context)).convo);
  await context.route("**/api/repos/*/agent-fixes", (route) => route.fulfill({ status: 403, json: { error: "reading a repository's agent fixes needs `diffs.read`" } }));
  const { convo } = await conversation(context);
  const fixes = convo.getByRole("link", { name: /^agent fixes: can't be read here: / });
  await expect(fixes).toHaveAttribute("title", /needs `diffs\.read`/);
  await expect(fixes).toHaveAttribute("data-unread", "");
  expect(await boxes(convo)).toEqual(before);
});
