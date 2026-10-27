/**
 * What to do with the agents selected in Mission control (docs/agents.md, "Mission control"), in the
 * production build against the mock: selected (checkboxes with a mouse; Select, then a tap on each,
 * by touch), the list's head offers one agent's own actions that some of them take, each saying how
 * many: Stop for the one working, Move to… (a machine that takes them all, or some, saying why the
 * rest stay), End… (asked in the line, with the agents they started); Pause and Resume for goals
 * running and paused. Each goes to each agent, a line says how it went and opens to each one's
 * outcome, and nothing scrolls sideways. On a desktop and a phone.
 */
import { expect, test, type Page } from "@playwright/test";

/** Against a real kiln server (playwright.config.ts): these drive the mock's agents. */
const REAL = !!process.env.E2E_BASE_URL;

/** Selects these agents' rows as the page lets one: by touch, Select, then a tap on each; with a mouse, their checkboxes. */
async function pick(page: Page, ...ids: string[]) {
  await expect(page.locator("li.ms-row").first()).toBeVisible();
  const touch = await page.evaluate(() => matchMedia("(pointer: coarse)").matches);
  if (touch) {
    const select = page.locator(".ms-listhead").getByRole("button", { name: "Select", exact: true });
    if (await select.isVisible()) await select.tap();
  }
  for (const id of ids) {
    const row = page.locator(`li.ms-row[data-agent="${id}"]`);
    if (touch) await row.locator(".ms-main").tap();
    else await row.getByRole("checkbox", { name: /^Select / }).check();
    await expect(row).toHaveAttribute("data-selected", "true");
  }
}

const bulk = (page: Page, n: number) => page.getByRole("group", { name: `What to do with the ${n} selected agent${n === 1 ? "" : "s"}` });
const said = (page: Page) => page.locator(".ms-bulk-said");

/** Nothing on the page, nor in the list's head, wider than the page. */
async function nothingSideways(page: Page) {
  const doc = await page.evaluate(() => ({ scroll: document.documentElement.scrollWidth, client: document.documentElement.clientWidth }));
  expect(doc.scroll, "the page scrolls sideways").toBeLessThanOrEqual(doc.client);
  const over = await page
    .locator(".ms-listhead *")
    .evaluateAll((els) => els.filter((e) => e.getBoundingClientRect().right > document.documentElement.clientWidth + 0.5).map((e) => `${e.tagName.toLowerCase()}.${e.className}`));
  expect(over, "something in the list's head is wider than the page").toEqual([]);
}

test("selected agents are stopped and moved at once, each as its own would be, and how it went opens to each", async ({ page }, info) => {
  test.skip(REAL, "the mock's agents");
  await page.goto("/agents");
  // "Fix the flaky retry test" works (Codex, on metal-ryzen-01); "Update the docs" is ready (Claude Code, on metal-ryzen-02).
  await pick(page, "fam-tests", "fam-docs");
  const bar = bulk(page, 2);
  await expect(bar).toBeVisible();
  await expect(bar.getByRole("button", { name: "Stop 1", exact: true })).toBeVisible();
  await expect(bar.getByRole("button", { name: "Move 2 to…", exact: true })).toBeVisible();
  await expect(bar.getByRole("button", { name: "End 2…", exact: true })).toBeVisible();
  // Neither has stopped, nor has goals to pause or resume.
  await expect(bar.getByRole("button", { name: /^(Restart|Remove|Resume|Pause) / })).toHaveCount(0);

  await bar.getByRole("button", { name: "Stop 1", exact: true }).click();
  await expect(said(page)).toContainText("Stopped the turns of 1 agent");
  const broadcasts = async () => ((await (await page.request.get("/api/mock/broadcasts")).json()) as { broadcasts: unknown[] }).broadcasts;
  await expect.poll(broadcasts).toContainEqual(expect.objectContaining({ to: { ids: ["fam-tests"] }, command: { type: "interrupt" } }));

  await bar.getByRole("button", { name: "Move 2 to…", exact: true }).click();
  const ask = page.getByRole("group", { name: "Move the selected agents" });
  const to = ask.getByRole("combobox", { name: "The machine they move to" });
  // A machine that takes only one says why the other stays.
  await to.selectOption({ label: "metal-pi-04 (1 of 2)" });
  await expect(ask).toContainText("One stays where it is: Update the docs (metal-pi-04 doesn't have Claude Code).");
  await to.selectOption({ label: "adacachy" });
  await expect(ask).not.toContainText("stays where it is");
  await nothingSideways(page);
  await page.screenshot({ path: info.outputPath(`mission-bulk-move-${info.project.name}.png`) });
  // One of them is at work: it goes once its turn is over (or now).
  await expect(ask.getByRole("button", { name: "Move now", exact: true })).toBeVisible();
  await ask.getByRole("button", { name: "Move after their turns", exact: true }).click();
  await expect(said(page)).toContainText("Moving 2 agents to adacachy, those at work once their turn is over");
  await expect(ask).toHaveCount(0);
  const moves = async () => (await (await page.request.get("/api/mock/moves")).json()) as unknown[];
  await expect.poll(moves).toEqual(
    expect.arrayContaining([
      { id: "fam-tests", to: "agent:adacachy", now: false },
      { id: "fam-docs", to: "agent:adacachy", now: false },
    ]),
  );
  await said(page)
    .getByRole("button", { name: /^Moving 2 agents/ })
    .click();
  await expect(page.locator(".ms-bulk-outcomes li")).toHaveCount(2);
  await expect(page.locator(".ms-bulk-outcomes li[data-result='done']")).toHaveCount(2);

  // Ending them is asked in the line first; neither started agents of its own.
  await bar.getByRole("button", { name: "End 2…", exact: true }).click();
  const end = page.getByRole("group", { name: "End the selected agents" });
  await expect(end).toContainText("End 2 agents? Their processes stop and they leave the board. Their changes stay on disk.");
  await expect(end.getByRole("checkbox")).toHaveCount(0);
  await nothingSideways(page);
  await page.screenshot({ path: info.outputPath(`mission-bulk-end-${info.project.name}.png`) });
  await end.getByRole("button", { name: "Cancel", exact: true }).click();
  await expect(end).toHaveCount(0);
  await expect(page.locator('li.ms-row[data-agent="fam-tests"]')).toHaveCount(1);
});

test("a selected agent's goals are paused and resumed, and ending it offers the agents it started", async ({ page }) => {
  test.skip(REAL, "the mock's agents");
  await page.goto("/agents");
  // "Release prep" runs its goals, and started three agents that still run.
  await pick(page, "fam-orch");
  const bar = bulk(page, 1);
  await bar.getByRole("button", { name: "Pause 1", exact: true }).click();
  await expect(said(page)).toContainText("Paused the goals of 1 agent: no next goal starts until you resume them");
  await bar.getByRole("button", { name: "Resume 1", exact: true }).click();
  await expect(said(page)).toContainText("Resumed 1 agent");
  await expect(bar.getByRole("button", { name: "Pause 1", exact: true })).toBeVisible();
  // It runs on the kiln server, where it stays.
  await expect(bar.getByRole("button", { name: /^Move / })).toHaveCount(0);

  await bar.getByRole("button", { name: "End 1…", exact: true }).click();
  const end = page.getByRole("group", { name: "End the selected agents" });
  const kids = end.getByRole("checkbox", { name: /^End the 3 agents it started too/ });
  await expect(kids).toBeChecked();
  for (const name of ["Fix the flaky retry test", "Update the docs", "Check the migration"]) await expect(end).toContainText(name);
  await end.getByRole("button", { name: "Cancel", exact: true }).click();
  await expect(end).toHaveCount(0);
  await nothingSideways(page);
});

test("ending selected agents ends each, and they leave the list and the selection", async ({ page }, info) => {
  test.skip(REAL, "the mock's agents");
  test.skip(info.project.name !== "desktop", "once: it ends the mock's agents for good");
  await page.goto("/agents");
  // The plan run's two steps' agents: "Step 1: Freeze main" and "Step 2: Run the full bench".
  await pick(page, "run-freeze", "run-bench");
  await bulk(page, 2).getByRole("button", { name: "End 2…", exact: true }).click();
  await page.getByRole("group", { name: "End the selected agents" }).getByRole("button", { name: "End 2", exact: true }).click();
  await expect(said(page)).toContainText("Ended 2 agents");
  await expect(page.locator('li.ms-row[data-agent="run-freeze"], li.ms-row[data-agent="run-bench"]')).toHaveCount(0);
  await expect(page.getByRole("group", { name: /^What to do with the/ })).toHaveCount(0);
});
