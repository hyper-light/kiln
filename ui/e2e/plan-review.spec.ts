/**
 * A plan waiting on you, reviewed in its agent's Plan tab (views/plans/review.tsx, docs/plans.md
 * "Reviewing a plan"), in the mock's three (mock/plan-review.ts): a plan a goal stack waits on with
 * light research ("CI/CD Integration"), one with heavy research ("Planner audit": 46 findings), and
 * one proposed in plan mode ("Slates"). Read step by step, research a few lines however much there
 * is, answered Approve, Reject or Other…; Other… brings it back changed, saying what changed; every
 * Review lands there; on its own page it's approved where it would be run; picking an answer moves
 * nothing; nothing scrolls sideways. On a desktop and a phone (and in WebKit, with E2E_WEBKIT=1).
 */
import { expect, test, type Page } from "@playwright/test";

test.beforeEach(async ({ page }) => {
  await page.request.post("/api/mock/plan-review/reset");
});

const wide = (page: Page) => page.evaluate(() => (document.scrollingElement?.scrollWidth ?? 0) > window.innerWidth);

/** The shell's cards for what other agents wait on (the mock has some) put away, as a person would: on a phone they sit over the page's foot. */
async function putAwayCards(page: Page) {
  const away = page.getByRole("list", { name: "Waiting on you" }).getByRole("button", { name: /^Put away/ });
  await away
    .first()
    .waitFor({ timeout: 3000 })
    .catch(() => undefined);
  // A phone shows one card at a time: wait for that card to leave before the next one,
  // rather than waiting for the whole queue to be empty after each single dismissal.
  while ((await away.count()) > 0) {
    const label = await away.first().getAttribute("aria-label");
    if (!label) throw new Error("A notice's dismiss button needs its accessible name");
    const card = page.getByRole("button", { name: label, exact: true });
    await card.click({ timeout: 1000 });
    await expect(card).toHaveCount(0);
  }
  await expect(away).toHaveCount(0);
}

test("a goal's plan is read and answered in its agent's Plan tab, and Other… brings it back changed", async ({ page }) => {
  await page.goto("/agents/rev-cicd?tab=plan");
  const review = page.locator(".pr");
  await expect(review.locator(".pr-eye")).toHaveText(/Needs you · a plan to review/);
  await expect(page.locator(".pr-tab-dot").first()).toBeVisible();
  await expect(review.locator(".pr-from")).toContainText("goal 6");
  await expect(review.locator(".pr-step")).toHaveCount(5);
  await expect(review.locator(".pr-round-label")).toHaveText(["2 at once · after 2"]);
  // Light research: nothing to look at, the least sure said.
  await expect(review.locator(".pr-rs")).toContainText("Nothing to look at");
  await expect(review.locator(".pr-rs")).toContainText("Least sure");

  // A step opens in place; Comment on it starts Other… with it named.
  await review.locator(".pr-step-head").first().click();
  const detail = review.locator(".pr-step").first().locator(".pr-detail");
  await expect(detail).toContainText("What to do");
  await expect(detail).toContainText("Done when");
  await expect(detail).toContainText("Rests on 2 findings");
  await detail.getByRole("button", { name: "Comment on step 1" }).click();
  const words = review.locator(".pr-words");
  await expect(words).toBeFocused();
  await expect(words).toHaveValue("Step 1 (Settle where the diff lives): ");
  await words.pressSequentially("it replaces Files: mount ReviewFiles under the lanes.");
  const send = review.locator(".pr-send");
  await expect(send).toHaveText("Send to CI/CD Integration");
  await send.click();

  // Sent: what you said, and what comes next; then back, a version on, saying what changed.
  const receipt = page.locator(".pr-receipt");
  await expect(receipt).toContainText("Sent to CI/CD Integration");
  await expect(receipt).toContainText("it replaces Files");
  await expect(page.locator(".pr-tab-dot")).toHaveCount(0);
  const delta = page.locator(".pr-delta");
  await expect(delta).toContainText("Changed since you looked", { timeout: 15_000 });
  await expect(delta).toContainText("You asked: Step 1 (Settle where the diff lives): it replaces Files");
  await expect(delta).toContainText("Step 1 rewritten: Mount ReviewFiles under the lanes (was: Settle where the diff lives)");
  await expect(delta).toContainText("Steps 3–5 unchanged");
  await expect(page.locator(".pr-when")).toContainText("v3");
  expect(await wide(page)).toBe(false);
});

test("heavy research stays a few lines: its counts and the findings worth a look, every one on asking", async ({ page }, info) => {
  await page.goto("/agents/rev-big?tab=plan");
  const rs = page.locator(".pr-rs");
  await expect(rs).toContainText("46 findings");
  await expect(rs.locator(".pr-grades .rs-grade")).toHaveCount(5);
  await expect(rs.locator(".pr-sub")).toHaveText("To look at before answering · 4");
  // Worst first: weak and unquoted, weak, contested; the fourth on asking.
  const look = rs.locator(":scope > .pr-fs > .pr-f");
  await expect(look).toHaveCount(3);
  await expect(look.locator(".pr-f-id")).toHaveText(["f23", "f9", "f12"]);
  await expect(look.first().locator(".pr-f-meta")).toContainText("weak, quote not found · step 5");
  await expect(rs).toContainText("Step 8 rests on no finding.");
  // However much research, its digest is a few lines.
  const tall = (await rs.boundingBox())?.height ?? 0;
  expect(tall, "the digest's height").toBeLessThan(info.project.name.startsWith("phone") ? 900 : 560);
  await rs.getByRole("button", { name: "1 more to look at" }).click();
  await expect(look).toHaveCount(4);

  // Every other finding, a line each, one opened at a time: why it's graded so, its quotes.
  await rs.getByRole("button", { name: "The other 42 findings" }).click();
  const others = rs.locator(".pr-all > .pr-fs").first().locator(".pr-f");
  await expect(others).toHaveCount(28);
  await rs.getByRole("button", { name: "14 no step rests on" }).click();
  await expect(rs.locator(".pr-all > .pr-fs")).toHaveCount(2);
  await others.first().locator(".pr-f-head").click();
  await expect(others.first().locator(".pr-f-open")).toContainText("of 100");
  await others.nth(1).locator(".pr-f-head").click();
  await expect(rs.locator(".pr-f-open")).toHaveCount(1);

  // A step resting on 12 findings: how they're graded, those worth a look; all on asking.
  const two = page.locator(".pr-step").nth(1);
  await two.locator(".pr-step-head").click();
  await expect(two.locator(".pr-k").filter({ hasText: "Rests on" })).toContainText("Rests on 12 findings");
  await expect(two.locator(".pr-f")).toHaveCount(2);
  await two.getByRole("button", { name: "All 12" }).click();
  await expect(two.locator(".pr-f")).toHaveCount(12);

  // Rejected: said so, and the stack waits.
  await page.locator(".pr-opt[data-v='reject'] .pr-row").click();
  await expect(page.locator(".pr-send")).toHaveText("Reject plan");
  await page.locator(".pr-send").click();
  await expect(page.locator(".pr-receipt")).toContainText("Rejected by you");
  await expect(page.locator(".pr-receipt")).toContainText("The stack waits for you");
  expect(await wide(page)).toBe(false);
});

test("a plan proposed in plan mode: a short card where it asked, read and answered in the Plan tab", async ({ page }, info) => {
  await page.goto("/agents/rev-mode");
  // On a phone the cards sit over the conversation's foot, where its card is.
  if (info.project.name.startsWith("phone")) await putAwayCards(page);
  const card = page.locator(".hx-asks .hx-approval");
  await expect(card.locator(".hx-plan-ask")).toHaveText("Fix: a mount taken over by a restarted daemon dropped a store");
  await expect(card.getByRole("button", { name: "Approve plan" })).toBeVisible();
  await card.getByRole("button", { name: "Review it" }).click();
  const review = page.locator(".pr");
  await expect(review.locator(".pr-eye")).toHaveText(/a proposed plan/);
  await expect(review.locator(".pr-step")).toHaveCount(5);
  await expect(page.getByText("Its plan waits on your review, above.")).toBeVisible();
  // A step opens to its words.
  await review.locator(".pr-step-head").first().click();
  await expect(review.locator(".pr-detail").first()).toContainText("fails 3 of 3");
  // Other…, by its key: it stays in plan mode, and proposes again, changed.
  await review.locator(".pr-opt[data-v='other'] .pr-row").click();
  await review.locator(".pr-words").fill("Write the failing tests first, before any fix.");
  await page.keyboard.press("ControlOrMeta+Enter");
  await expect(page.locator(".pr-receipt")).toContainText("Sent to Slates");
  const delta = page.locator(".pr-delta");
  await expect(delta).toContainText("Changed since you looked", { timeout: 15_000 });
  await expect(delta).toContainText("You asked: Write the failing tests first, before any fix.");
  await expect(delta).toContainText("Step 2 added: Tests, red first");
  expect(await wide(page)).toBe(false);
});

test("every Review lands in the agent's Plan tab: Mission control's row, and the goal on its stack", async ({ page }, info) => {
  await page.goto("/agents");
  const row = page.locator(".ms-row[data-agent='rev-cicd']");
  await expect(row.getByRole("button", { name: "Approve" })).toBeVisible();
  // A desktop's row says Review; a phone's opens by its arrow, there too.
  if (info.project.name.startsWith("phone")) await row.getByRole("button", { name: "Open CI/CD Integration" }).click();
  else await row.getByRole("link", { name: "Review" }).click();
  await expect(page).toHaveURL(/\/agents\/rev-cicd\?tab=plan$/);
  await expect(page.locator(".pr")).toBeVisible();
  // The goal it was made from says so, and brings the review back into view.
  const goal = page.locator(".gs-review-it");
  await goal.scrollIntoViewIfNeeded();
  await expect(page.locator(".gs-review")).toHaveText("waits for your review");
  await goal.click();
  await expect(page.locator(".pr-top")).toBeInViewport({ timeout: 5_000 });
});

test("a plan its goals wait on is approved on its page, where it would be run, and its agent needs you no longer", async ({ page }, info) => {
  await page.goto("/agents/plans/pl_cicd");
  // On a phone the cards sit over the page's foot.
  if (info.project.name.startsWith("phone")) await putAwayCards(page);
  // Approve in Run's place: it runs no other way, and its steps aren't run on their own.
  const approve = page.getByRole("region", { name: "Approve the plan" });
  await expect(approve).toContainText("CI/CD Integration’s goals wait on your review of this plan. Approved, its 5 steps go to CI/CD Integration as its goal’s parts: it runs no other way.");
  await expect(approve.getByRole("link", { name: "review it in its Plan tab" })).toHaveAttribute("href", "/agents/rev-cicd?tab=plan");
  await expect(page.getByRole("region", { name: "Run the plan" })).toHaveCount(0);
  await expect(page.locator(".sv-hint")).toHaveText("Pick steps to see what they are");
  await approve.getByRole("button", { name: "Approve" }).click();
  await expect(approve).toContainText("Approved: its 5 steps go to CI/CD Integration as its goal’s parts");
  expect(await wide(page)).toBe(false);
  // Its row in Mission control goes on, nothing waiting on you.
  await page.goto("/agents");
  const row = page.locator(".ms-row[data-agent='rev-cicd']");
  await expect(row.locator(".ms-word")).toHaveText("Working");
  await expect(row.getByRole("button", { name: "Approve" })).toHaveCount(0);
});

test("picking an answer moves nothing above it, and its button keeps its width", async ({ page }) => {
  await page.goto("/agents/rev-cicd?tab=plan");
  const review = page.locator(".pr");
  await expect(review.locator(".pr-step")).toHaveCount(5);
  // From the review's own top: its tray scrolling (as a row is brought into view to click it) moves nothing.
  const boxes = () =>
    page.evaluate(() => {
      const top = document.querySelector(".pr")?.getBoundingClientRect().top ?? 0;
      const at = (sel: string) => {
        const b = document.querySelector(sel)?.getBoundingClientRect();
        return b ? [Math.round(b.top - top), Math.round(b.height), Math.round(b.width)] : null;
      };
      return { title: at(".pr-title"), steps: at(".pr-steps"), rs: at(".pr-rs"), approve: at(".pr-opt[data-v='approve']"), send: at(".pr-send")?.[2] };
    });
  const before = await boxes();
  for (const v of ["approve", "reject", "approve"]) {
    await review.locator(`.pr-opt[data-v='${v}'] .pr-row`).click();
    const now = await boxes();
    expect({ ...now, approve: v === "approve" ? now.approve : before.approve }).toEqual(before);
  }
});
