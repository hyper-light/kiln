/**
 * An agent's subagents in motion, answered and removed, wherever their tree is (Mission control's,
 * the composer's): one at work turns its ring through the prism (every ring on one clock; still
 * when motion is reduced), the path runs on unbroken past what one asks, a question is answered
 * right under its row in Mission control, and one that ended is removed as kiln's Remove removes an
 * agent: its trash, a swipe by touch, or Remove for those picked by the composer. Against the mock
 * (Storage lead's tree, Gate order's question); what's removed is asked, not done, so the mock
 * stays as every other test finds it.
 */
import { expect, test, type Page, type Request } from "@playwright/test";

const REAL = !!process.env.E2E_BASE_URL;

test.beforeEach(async ({ request }) => {
  test.skip(REAL, "the mock's agents");
  await request.post("/api/mock/tree-asks");
});

test.afterEach(async ({ request }) => {
  if (!REAL) await request.post("/api/mock/asks");
});

/** Requests to remove one (a session's command, or its session's end), taken here and answered as done: the mock keeps it. */
async function holdRemoves(page: Page): Promise<Request[]> {
  const seen: Request[] = [];
  await page.route(/\/api\/harness\/sessions\/[^/]+(\/command)?$/, async (route) => {
    const r = route.request();
    const remove = r.method() === "DELETE" || (r.method() === "POST" && (r.postDataJSON() as { type?: string } | null)?.type === "remove_task");
    if (!remove) return route.fallback();
    seen.push(r);
    await route.fulfill({ status: 200, contentType: "application/json", body: "{}" });
  });
  return seen;
}

/** The composer's tree on Storage lead's page, open. */
async function dockTree(page: Page) {
  await page.goto("/agents/tree-lead");
  await page.locator(".al-line").click();
  await expect(page.locator(".dn").first()).toBeVisible();
}

const turning = (page: Page, rows: string) =>
  page.locator(rows).evaluateAll((els) => els.map((el) => el.getAnimations({ subtree: true }).filter((a) => (a as CSSAnimation).animationName === "tp-turn" && a.playState === "running").length));

test("one at work turns its ring, on one clock; the rest stay still", async ({ page }) => {
  await dockTree(page);
  const working = await turning(page, '.dn[data-stand="working"]');
  expect(working.length).toBeGreaterThan(1);
  expect(working.every((n) => n === 1)).toBe(true);
  expect((await turning(page, '.dn:not([data-stand="working"])')).every((n) => n === 0)).toBe(true);
  // One clock: every ring where every other one is in its turn.
  const times = await page.locator('.dn[data-stand="working"] .tp-turn').evaluateAll((els) => els.map((el) => Math.round(Number(el.getAnimations()[0]?.currentTime ?? -1) / 50)));
  expect(new Set(times).size).toBe(1);
  // Its word carries the prism, as Mission control's own working rows' do.
  await expect(page.locator('.dn[data-stand="working"] .dn-word.sg-shimmer').first()).toBeVisible();
  await page.goto("/agents");
  await page.locator('.ms-row[data-agent="tree-lead"]').getByRole("button", { name: "Show Storage lead's subagents" }).click();
  expect((await turning(page, '.ms-node[data-stand="working"]')).every((n) => n === 1)).toBe(true);
});

test("still when motion is reduced", async ({ page }) => {
  await page.emulateMedia({ reducedMotion: "reduce" });
  await dockTree(page);
  expect((await turning(page, ".dn")).every((n) => n === 0)).toBe(true);
});

test("the path runs on unbroken past what one asks, to the rows below", async ({ page }) => {
  await dockTree(page);
  const needs = page.locator('.dn[data-stand="needs"]').first();
  await expect(needs.locator(".dn-answer")).toBeVisible();
  const gap = await needs.evaluate((li) => {
    const svg = li.querySelector("svg.tp");
    const box = li.getBoundingClientRect();
    return svg ? Math.abs(box.bottom - parseFloat(getComputedStyle(li).borderBottomWidth) - svg.getBoundingClientRect().bottom) : 99;
  });
  expect(gap).toBeLessThan(0.5);
});

test("one that ended is removed by its trash, where its pause was", async ({ page }) => {
  const seen = await holdRemoves(page);
  await dockTree(page);
  const row = page.locator(".dn", { has: page.locator(".dn-name", { hasText: "Port the lexer" }) });
  await expect(row.getByRole("button", { name: "Pause Port the lexer" })).toHaveCount(0);
  await row.getByRole("button", { name: "Remove Port the lexer" }).click();
  await expect(row).toHaveCount(0);
  expect(seen.map((r) => r.postDataJSON())).toEqual([{ type: "remove_task", task: "tl-h1" }]);
  // One kiln started: its session goes, as Mission control's Remove ends an agent's.
  await page
    .locator(".dn", { has: page.locator(".dn-name", { hasText: "Docs pass" }) })
    .getByRole("button", { name: "Remove Docs pass" })
    .click();
  await expect.poll(() => seen.length).toBe(2);
  expect(seen[1]?.method()).toBe("DELETE");
  expect(seen[1]?.url()).toMatch(/\/api\/harness\/sessions\/tree-s5$/);
});

test("Remove, by the tree's head, removes those picked that ended and keeps the rest picked", async ({ page }) => {
  const seen = await holdRemoves(page);
  await dockTree(page);
  const top = page.locator(".al-top");
  await expect(top.getByRole("button", { name: "Remove" })).toHaveCount(0);
  await page.getByRole("checkbox", { name: "Pick Leader election over QUIC" }).check();
  await expect(top.getByRole("button", { name: "Remove" })).toBeDisabled();
  await page.getByRole("checkbox", { name: "Pick Bench the follower" }).check();
  await top.getByRole("button", { name: "Remove" }).click();
  await expect(page.locator(".dn", { has: page.locator(".dn-name", { hasText: "Bench the follower" }) })).toHaveCount(0);
  expect(seen.map((r) => r.postDataJSON())).toEqual([{ type: "remove_task", task: "tl-h2" }]);
  await expect(page.getByRole("checkbox", { name: "Pick Leader election over QUIC" })).toBeChecked();
});

test("by touch, one that ended swipes away; one at work stays put", async ({ page, hasTouch }) => {
  test.skip(!hasTouch, "a touch screen's");
  const seen = await holdRemoves(page);
  await dockTree(page);
  const swipe = async (name: string) => {
    const main = page.locator(".dn", { has: page.locator(".dn-name", { hasText: name }) }).locator(".dn-main");
    await main.scrollIntoViewIfNeeded();
    const box = await main.boundingBox();
    if (!box) throw new Error(`no ${name}`);
    const at = { x: box.x + 20, y: box.y + 10 };
    const fire = (type: string, x: number) => main.dispatchEvent(type, { pointerType: "touch", pointerId: 7, isPrimary: true, clientX: x, clientY: at.y, bubbles: true });
    await fire("pointerdown", at.x);
    for (const dx of [12, 40, 80, 120]) await fire("pointermove", at.x + dx);
    await fire("pointerup", at.x + 120);
  };
  await swipe("Port the lexer");
  await expect(page.locator(".dn", { has: page.locator(".dn-name", { hasText: "Port the lexer" }) })).toHaveCount(0);
  expect(seen.map((r) => r.postDataJSON())).toEqual([{ type: "remove_task", task: "tl-h1" }]);
  await swipe("Leader election over QUIC");
  await expect(page.locator(".dn", { has: page.locator(".dn-name", { hasText: "Leader election over QUIC" }) })).toHaveCount(1);
  expect(seen).toHaveLength(1);
});

test("Mission control: a question is answered right under its row, and the row goes on", async ({ page }) => {
  await page.goto("/agents");
  const row = page.locator('.ms-row[data-agent="ask-lead"]');
  const answerButton = row.getByRole("button", { name: "Answer" });
  await answerButton.click();
  await expect(answerButton).toHaveAttribute("aria-expanded", "true");
  const asked = row.locator(".ms-answer");
  await expect(asked.getByRole("radio").first()).toBeVisible();
  await expect(page).toHaveURL(/\/agents$/);
  const sent = page.waitForRequest((r) => r.method() === "POST" && /\/api\/harness\/sessions\/[^/]+\/command$/.test(r.url()) && (r.postDataJSON() as { type?: string }).type === "approve");
  await asked.getByRole("radio").first().click();
  await asked.getByRole("button", { name: "Submit" }).click();
  const c = (await sent).postDataJSON() as { type: string; decision: string; choices?: { selected: string[] }[] };
  expect(c.decision).toBe("allow");
  expect(c.choices?.[0]?.selected.length).toBe(1);
  await expect(row).not.toHaveAttribute("data-stand", "needs");
});

test("Mission control: a subagent that ended is removed from its tree", async ({ page }) => {
  const seen = await holdRemoves(page);
  await page.goto("/agents");
  await page.locator('.ms-row[data-agent="tree-lead"]').getByRole("button", { name: "Show Storage lead's subagents" }).click();
  const node = page.locator(".ms-node", { has: page.locator(".ms-node-name", { hasText: "Bench the follower" }) });
  await node.getByRole("button", { name: "Remove Bench the follower" }).click();
  await expect(node).toHaveCount(0);
  expect(seen.map((r) => r.postDataJSON())).toEqual([{ type: "remove_task", task: "tl-h2" }]);
});
