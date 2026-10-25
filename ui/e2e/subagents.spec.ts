/**
 * Messages to a subagent (docs/agents.md, "Messages to its subagents"), in the production build
 * against the mock: with a subagent's thread open, the agent page's composer writes to that
 * subagent as its harness lets one in (`Task.takes`). A Claude Code subagent at work in the
 * background is steered or sent a queued message by way of its agent; a Codex one is sent one now
 * on its own thread; one that finished takes one for when it's resumed; one stopped leaves the
 * composer to its agent. Each message shows in the subagent's thread, one queued waits above the
 * field (and goes from there), and nothing scrolls sideways on a phone or a desktop.
 */
import { expect, test, type Page } from "@playwright/test";

/** Against a real kiln server (playwright.config.ts): these drive the mock's agents. */
const REAL = !!process.env.E2E_BASE_URL;

/** The commands the page sends agents' sessions, as it sends them. */
function commands(page: Page): Record<string, unknown>[] {
  const sent: Record<string, unknown>[] = [];
  page.on("websocket", (ws) => {
    if (!ws.url().includes("/api/harness/sessions/")) return;
    ws.on("framesent", (f) => {
      try {
        const v: unknown = JSON.parse(String(f.payload));
        if (v && typeof v === "object") sent.push(v as Record<string, unknown>);
      } catch {
        // Not a command.
      }
    });
  });
  return sent;
}

const PICK = 'select[aria-label="How to send while it works"]';
/** The ways to send it offers (its narrow pick has them all, shown or not). */
const offered = (page: Page) => page.locator(PICK).locator("option").allTextContents();
/** How to send, chosen as the page shows it: its row of choices, or its pick on a phone. */
async function choose(page: Page, mode: "Queue" | "Steer" | "Now") {
  const radio = page.getByRole("radio", { name: mode, exact: true });
  if (await radio.isVisible()) await radio.click();
  else await page.locator(PICK).selectOption({ label: mode });
}
const said = (page: Page, text: string) => page.locator(".hx-prompt", { hasText: text });
const sendButton = (page: Page) => page.locator("button.cp-send");

/** The shell's cards for what other agents wait on (the mock has some) put away, as a person would: on a phone they sit over the composer. */
async function putAwayCards(page: Page) {
  const away = page.getByRole("list", { name: "Waiting on you" }).getByRole("button", { name: /^Put away/ });
  await away
    .first()
    .waitFor({ timeout: 3000 })
    .catch(() => undefined);
  await expect(async () => {
    if ((await away.count()) > 0) await away.first().click({ timeout: 1000 });
    await expect(away).toHaveCount(0, { timeout: 500 });
  }).toPass({ timeout: 10_000 });
}

/** Nothing on the page, nor in the composer, wider than the page. */
async function nothingSideways(page: Page) {
  const doc = await page.evaluate(() => ({ scroll: document.documentElement.scrollWidth, client: document.documentElement.clientWidth }));
  expect(doc.scroll, "the page scrolls sideways").toBeLessThanOrEqual(doc.client);
  const over = await page
    .locator(".hx-compose-box *, .qd *")
    .evaluateAll((els) => els.filter((e) => e.getBoundingClientRect().right > document.documentElement.clientWidth + 0.5).map((e) => `${e.tagName.toLowerCase()}.${e.className}`));
  expect(over, "something in the composer is wider than the page").toEqual([]);
}

test("a Claude Code subagent at work in the background is steered, or sent a queued message, by way of its agent", async ({ page }, info) => {
  test.skip(REAL, "the mock's agents");
  const sent = commands(page);
  const steer = `Use the v2 parser (${info.project.name})`;
  const later = `Then run the figure tests (${info.project.name})`;
  await page.goto("/agents/sub-busy?task=a6f2e91c0b7d4e5f8");
  const box = page.getByRole("textbox", { name: "Message to the subagent" });
  await expect(box).toHaveAttribute("placeholder", "Queue a message for when the subagent finishes (the main agent passes it on)");
  await putAwayCards(page);
  expect(await offered(page)).toEqual(["Queue", "Steer"]);
  await expect(page.getByRole("button", { name: "Stop the subagent" })).toBeVisible();

  await choose(page, "Steer");
  await expect(box).toHaveAttribute("placeholder", "Steer the subagent — it reads this at its next step (the main agent passes it on)");
  await box.fill(steer);
  await expect(sendButton(page)).toHaveAccessibleName("Steer");
  await sendButton(page).click();
  await expect(said(page, steer)).toBeVisible();
  await expect.poll(() => sent).toContainEqual(expect.objectContaining({ type: "send", text: steer, mode: "steer", task: "a6f2e91c0b7d4e5f8" }));

  // Queued, it waits above the field for the subagent to finish, and can be steered in from there.
  await choose(page, "Queue");
  await box.fill(later);
  await expect(sendButton(page)).toHaveAccessibleName("Queue");
  await sendButton(page).click();
  const queued = page.locator(".qd", { hasText: "Queued" });
  await expect(queued).toContainText(later);
  await queued.locator(".qd-line").click();
  await expect(queued).toContainText("sent in order when the subagent finishes");
  const row = queued.locator("li.qd-row", { hasText: later });
  await expect(row.getByRole("button", { name: "Steer now" })).toBeVisible();
  await expect(row.getByRole("button", { name: "Send now" })).toHaveCount(0);
  await nothingSideways(page);
  await page.screenshot({ path: info.outputPath(`subagent-claude-${info.project.name}.png`) });
  await row.getByRole("button", { name: "Steer now" }).click();
  await expect(said(page, later)).toBeVisible();
  await expect(queued.locator("li.qd-row", { hasText: later })).toHaveCount(0);
  await expect.poll(() => sent).toContainEqual(expect.objectContaining({ type: "queue_steer" }));
  await expect(said(page, steer)).toHaveCount(1);
});

test("a Codex subagent at work is sent a message now, on its own thread", async ({ page }, info) => {
  test.skip(REAL, "the mock's agents");
  const sent = commands(page);
  const now = `Look at the jitter too (${info.project.name})`;
  const later = `And the retry cap (${info.project.name})`;
  await page.goto("/agents/sub-codex?task=019a-child");
  const box = page.getByRole("textbox", { name: "Message to the subagent" });
  await expect(box).toHaveAttribute("placeholder", "Queue a message for when the subagent finishes");
  await putAwayCards(page);
  expect(await offered(page)).toEqual(["Queue", "Steer", "Now"]);

  await choose(page, "Now");
  await expect(box).toHaveAttribute("placeholder", "Stop the subagent and say…");
  await box.fill(now);
  await expect(sendButton(page)).toHaveAccessibleName("Send now");
  await sendButton(page).click();
  await expect(said(page, now)).toBeVisible();
  await expect.poll(() => sent).toContainEqual(expect.objectContaining({ type: "send", text: now, mode: "now", task: "019a-child" }));

  // One queued for it can be sent now from where it waits.
  await choose(page, "Queue");
  await box.fill(later);
  await sendButton(page).click();
  const queued = page.locator(".qd", { hasText: "Queued" });
  await queued.locator(".qd-line").click();
  const row = queued.locator("li.qd-row", { hasText: later });
  await expect(row.getByRole("button", { name: "Steer now" })).toBeVisible();
  await nothingSideways(page);
  await page.screenshot({ path: info.outputPath(`subagent-codex-${info.project.name}.png`) });
  await row.getByRole("button", { name: "Send now" }).click();
  await expect(said(page, later)).toBeVisible();
  await expect.poll(() => sent).toContainEqual(expect.objectContaining({ type: "queue_send_now" }));
});

test("a Claude Code subagent that finished takes a message for when it's resumed", async ({ page }, info) => {
  test.skip(REAL, "the mock's agents");
  const sent = commands(page);
  const text = `Add the scheduler's two fixes (${info.project.name})`;
  await page.goto("/agents/sub-done?task=c1");
  const box = page.getByRole("textbox", { name: "Message to the subagent" });
  await expect(box).toHaveAttribute("placeholder", "Message the subagent (the main agent passes it on)");
  await putAwayCards(page);
  await expect(page.locator(PICK)).toHaveCount(0);
  await expect(page.getByRole("button", { name: "Stop the subagent" })).toHaveCount(0);
  await box.fill(text);
  await expect(sendButton(page)).toHaveAccessibleName("Send");
  await sendButton(page).click();
  await expect(said(page, text)).toBeVisible();
  await expect.poll(() => sent).toContainEqual(expect.objectContaining({ type: "send", text, mode: "queue", task: "c1" }));
  await nothingSideways(page);
});

test("Stop stops the subagent shown, and one that takes no message leaves the composer to its agent", async ({ page }, info) => {
  test.skip(REAL, "the mock's agents");
  test.skip(info.project.name !== "desktop", "once: it stops the mock's subagent for good");
  const sent = commands(page);
  await page.goto("/agents/fam-orch?task=a-planner");
  // At work in the foreground (its agent's turn waits on it): a message waits for it to finish.
  await expect(page.getByRole("textbox", { name: "Message to the subagent" })).toHaveAttribute("placeholder", "Queue a message for when the subagent finishes (the main agent passes it on)");
  await putAwayCards(page);
  await expect(page.locator(PICK)).toHaveCount(0);
  await page.getByRole("button", { name: "Stop the subagent" }).click();
  await expect.poll(() => sent).toContainEqual(expect.objectContaining({ type: "stop_task", task: "a-planner" }));
  // Claude Code doesn't resume a subagent it stopped: what's written goes to the main agent again.
  await expect(page.getByRole("textbox", { name: "Message to the agent" })).toHaveAttribute("placeholder", "Message the main agent");
  await expect(page.getByRole("button", { name: "Stop the subagent" })).toHaveCount(0);
});
