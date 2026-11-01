// W26-R18: a repository's Review section in Repos holds the switch that lets agents fix code from
// agents' feedback: off until turned on, what it does in plain words, who set it and when; disabled,
// saying why, for someone who can't change it; and nothing on the page moves when it's turned.
import { expect, test, type Locator, type Page } from "@playwright/test";

/** A repository of this project's own, so each starts with the switch as kiln does: off, set by no one. */
async function open(page: Page, repo: string): Promise<Locator> {
  await page.request.post("/api/repos", { data: { name: repo } });
  await page.goto("/repos");
  await page.getByText(repo).first().click();
  return page.getByRole("region", { name: "Review" });
}

/** The section's size and where each of its rows is within it, to the pixel (clicking may scroll
 * the page, which moves nothing). */
async function rows(review: Locator) {
  return review.evaluate((s) => {
    const at = s.getBoundingClientRect();
    return [s, ...s.querySelectorAll("button, p")].map((e) => {
      const r = e.getBoundingClientRect();
      return [r.left - at.left, r.top - at.top, r.width, r.height].map(Math.round);
    });
  });
}

test("a repository's Review section turns agents' fixes on and off, saying what it does, who and when, and nothing moves (W26-R18)", async ({ page }, info) => {
  const review = await open(page, `acme/fixes-${info.project.name}`);
  const turn = review.getByRole("switch", { name: "Agents fix from agents' feedback" });
  await expect(turn).toHaveAttribute("aria-checked", "false");
  await expect(turn).toBeEnabled();
  await expect(review.getByText("On: an agent's comment on an agent's change goes to that change's agent, which pushes a fix and answers in the thread.", { exact: false })).toBeVisible();
  await expect(review.getByText("Off: agents' comments wait for a person, who can Hand off.", { exact: false })).toBeVisible();
  await expect(review.getByText("No one has turned it on.")).toBeVisible();
  const before = await rows(review);

  await turn.click();
  await expect(turn).toHaveAttribute("aria-checked", "true");
  await expect(review.getByText(/^Turned on by the owner, \d{1,2} [A-Z][a-z]{2,3} \d{4}\.$/)).toBeVisible();
  expect(await rows(review)).toEqual(before);

  await turn.click();
  await expect(turn).toHaveAttribute("aria-checked", "false");
  await expect(review.getByText(/^Turned off by the owner, /)).toBeVisible();
  expect(await rows(review)).toEqual(before);
});

test("someone who can't land a repository's changes sees its switch disabled, saying why (W26-R18)", async ({ page }, info) => {
  await page.route("**/api/repos/*/agent-fixes", async (route) => {
    const r = await route.fetch();
    // A reader's answer: set by ada at noon on 31 Oct 2026, which they may not change.
    await route.fulfill({ response: r, json: { ...(await r.json()), agents_fix: true, set_by: "user:ada", set_at: 1793448000, may_change: false } });
  });
  const review = await open(page, `acme/read-${info.project.name}`);
  const turn = review.getByRole("switch", { name: "Agents fix from agents' feedback" });
  await expect(turn).toHaveAttribute("aria-checked", "true");
  await expect(turn).toBeDisabled();
  await expect(review.getByText("Turned on by ada, 31 Oct 2026.")).toBeVisible();
  await expect(review.getByText("Only someone who can land its changes (diffs.land) turns it on or off.")).toBeVisible();
});
