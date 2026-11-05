/**
 * The card asking about code (components/code/about.tsx) opens right under the line tapped, and
 * every diff in Ship has it (Ada, 2026-11-04: "Our diffs view/PR view in Ship does NOT show the
 * definition/about/ask/copy blocks like in the agent chat diff. Likewise, both are wildly
 * innacurate about line positioning"). In an agent's Changes (views/harness/diff.tsx), a hunk with
 * lines removed and added, unified on a phone and side by side where there's room, puts the card
 * under the line, not under the hunk's last. In Ship, a host's pull request no agent made asks a
 * helper kiln starts for the person (`ci.repos.about`), its answer streaming into the card.
 */
import { expect, test, type Locator } from "@playwright/test";
import { pickCommit } from "./ship-pick";

const REAL = !!process.env.E2E_BASE_URL;
test.skip(REAL, "drives the mock's agents and commits");

/** Where `word` is drawn in an element's text, to tap it there. */
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

/** Taps `word` in `code`; a tap something else would take fails rather than lands there. */
async function tap(code: Locator, word: string) {
  await code.evaluate((e) => e.scrollIntoView({ block: "center" }));
  const [at, box] = await Promise.all([wordIn(code, word), code.boundingBox()]);
  expect(at && box, `"${word}" is drawn`).toBeTruthy();
  await code.click({ position: { x: (at?.x ?? 0) - (box?.x ?? 0), y: (at?.y ?? 0) - (box?.y ?? 0) } });
}

/** A still for the person, when asked for (E2E_STILLS: a folder), as ship-review.spec takes them:
 * once `card` has slid open, what it settles to rather than a frame of its slide. */
const still = async (page: import("@playwright/test").Page, card: Locator, name: string) => {
  if (!process.env.E2E_STILLS) return;
  await card.evaluate((el) => Promise.all((el.closest(".hx-about-card") ?? el).getAnimations({ subtree: true }).map((a) => a.finished.catch(() => undefined))));
  await page.screenshot({ path: `${process.env.E2E_STILLS}/${name}-${test.info().project.name}.png` });
};

/** How far the card's top is from the line's bottom, in pixels: 0 when it's right under it. */
const gap = async (line: Locator, card: Locator) => {
  const [l, c] = await Promise.all([line.boundingBox(), card.boundingBox()]);
  return l && c ? Math.abs(Math.round(c.y - (l.y + l.height))) : null;
};

test("a name tapped in an agent's change opens its card right under that line, not under the hunk's last", async ({ page }) => {
  await page.goto("/agents/fam-orch?tab=changes");
  // The mock's agents waiting on you lie over the inspector's lower half: out of the way, as ship-deploys.spec does.
  await page.addStyleTag({ content: ".rq-viewport { display: none !important; }" });
  await page
    .getByRole("tab", { name: /^Changes/ })
    .filter({ visible: true })
    .first()
    .click();
  const diff = page.locator(".hx-diff:visible", { hasText: "planner.rs" }).first();
  await expect(diff).toBeVisible();
  // An added line in a hunk that removes a line too: the hunk's last line is two further down.
  const row = diff.locator(".hx-row[data-kind=add]", { hasText: "let ready = order" }).first();
  await tap(row.locator(".hx-code"), "ready");
  const card = diff.getByRole("region", { name: "About ready" });
  await expect(card).toBeVisible();
  await expect(row).toHaveAttribute("data-asked", "true");
  await expect.poll(() => gap(row, diff.locator(".hx-about-card").first())).toBeLessThanOrEqual(1);
  await still(page, card, "about-agent-changes");
  // The lines under it come after the card: the hunk's last line is below it.
  const last = diff.locator(".hx-row", { hasText: "first.map(Step::start)" }).first();
  const [c, l] = await Promise.all([card.boundingBox(), last.boundingBox()]);
  expect(c && l && l.y >= c.y + c.height - 1).toBeTruthy();
  await card.getByRole("button", { name: "Close" }).click();
  await expect(diff.getByRole("region", { name: "About ready" })).toHaveCount(0);
});

test("a host's pull request no agent made has the card too: kiln's helper answers, under the line tapped", async ({ page }) => {
  await page.goto("/ship");
  const row = await pickCommit(page, "Faster builds");
  const files = row.getByRole("region", { name: "Files", exact: true });
  const line = files.locator(".rv-l", { hasText: "echo running" }).first();
  await expect(line.locator(".rv-t[data-ask]")).toHaveCount(1);
  await tap(line.locator(".rv-t"), "running");
  const card = files.getByRole("region", { name: "About running" });
  await expect(card).toBeVisible();
  await expect.poll(() => gap(line, files.locator(".hx-about-card").first())).toBeLessThanOrEqual(1);
  await expect(card).toContainText("kiln starts a helper that reads the repository");
  await expect(card.getByRole("button", { name: "Copy" })).toBeVisible();
  await card.getByRole("button", { name: "About", exact: true }).click();
  await expect(card).toContainText("a helper kiln started");
  await expect(card).toContainText("What it is", { ignoreCase: true, timeout: 15_000 });
  await still(page, card, "about-host-pull");
  await card.getByRole("button", { name: "Close" }).click();
  await expect(files.getByRole("region", { name: "About running" })).toHaveCount(0);
});
