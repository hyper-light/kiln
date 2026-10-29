/**
 * What to do with the agents selected in Mission control (docs/agents.md, "Mission control"), in the
 * production build against the mock: selected (checkboxes with a mouse; Select, then a tap on each,
 * by touch), the list's head is one line from the moment selecting starts: a ring (or checkbox) that
 * picks them all, how many, the Actions menu and Clear. The menu has one agent's own actions, turned
 * off where none of them takes one and saying how many each is for when not all: Resume and Pause for
 * goals paused and running, Move to… (a machine that takes them all, or some, saying why the rest
 * stay), Stop their turns, End… (asked in the line, with the agents they started). Each goes to each
 * agent, a line says how it went and opens to each one's outcome. Clear, or putting back the last one,
 * ends selecting. Picking more moves nothing (the list, the head, its buttons, the composer's one-line
 * To line), and nothing scrolls sideways. On a desktop and a phone.
 */
import { expect, test, type Page } from "@playwright/test";

/** Against a real kiln server (playwright.config.ts): these drive the mock's agents. */
const REAL = !!process.env.E2E_BASE_URL;

const touchScreen = (page: Page) => page.evaluate(() => matchMedia("(pointer: coarse)").matches);
const rowOf = (page: Page, id: string) => page.locator(`li.ms-row[data-agent="${id}"]`);
const head = (page: Page) => page.locator(".ms-listhead");

/** Selects these agents' rows as the page lets one: by touch, Select, then a tap on each; with a mouse, their checkboxes. */
async function pick(page: Page, ...ids: string[]) {
  await expect(page.locator("li.ms-row").first()).toBeVisible();
  const touch = await touchScreen(page);
  if (touch) {
    const select = head(page).getByRole("button", { name: "Select", exact: true });
    if (await select.isVisible()) await select.tap();
  }
  for (const id of ids) {
    const row = rowOf(page, id);
    if (touch) await row.locator(".ms-main").tap();
    else await row.getByRole("checkbox", { name: /^Select / }).check();
    await expect(row).toHaveAttribute("data-selected", "true");
  }
}

/** Puts one back as the page lets one: a tap by touch, its checkbox with a mouse. */
async function putBack(page: Page, id: string) {
  const row = rowOf(page, id);
  if (await touchScreen(page)) await row.locator(".ms-main").tap();
  else await row.getByRole("checkbox", { name: /^Select / }).uncheck();
  await expect(row).not.toHaveAttribute("data-selected", "true");
}

const bulk = (page: Page, n: number) => page.getByRole("group", { name: `What to do with the ${n} selected agent${n === 1 ? "" : "s"}` });
const said = (page: Page) => page.locator(".ms-bulk-said");
/** An action in the Actions menu (its name ends with how many it's for, when not all). */
const action = (page: Page, name: string) => page.getByRole("menuitem", { name: new RegExp(`^${name.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}`) });

/** Opens the Actions menu, does `go` with it, and leaves it shut. */
async function inMenu(page: Page, go: () => Promise<void>) {
  await head(page).getByRole("button", { name: "Actions" }).click();
  await expect(page.getByRole("menu")).toBeVisible();
  await go();
  if (await page.getByRole("menu").isVisible()) await page.keyboard.press("Escape");
  await expect(page.getByRole("menu")).toHaveCount(0);
}

/** Nothing on the page, nor in the list's head, wider than the page. */
async function nothingSideways(page: Page) {
  const doc = await page.evaluate(() => ({ scroll: document.documentElement.scrollWidth, client: document.documentElement.clientWidth }));
  expect(doc.scroll, "the page scrolls sideways").toBeLessThanOrEqual(doc.client);
  const over = await page
    .locator(".ms-listhead *")
    .evaluateAll((els) => els.filter((e) => e.getBoundingClientRect().right > document.documentElement.clientWidth + 0.5).map((e) => `${e.tagName.toLowerCase()}.${e.className}`));
  expect(over, "something in the list's head is wider than the page").toEqual([]);
}

/**
 * Where things are: a row of the list from the page's top (scrolling doesn't count, moving does), how
 * big the list's head and the composer's To line are, and each of the head's buttons from its top (the
 * head is held in view by touch while selecting, so where it is on the page goes with the scroll).
 */
function places(page: Page, row: string) {
  return page.evaluate((row) => {
    const head = document.querySelector(".ms-listhead");
    const top = head?.getBoundingClientRect().top ?? 0;
    const inHead = (el: Element | null) => {
      if (!el) return null;
      const b = el.getBoundingClientRect();
      return `${Math.round(b.x)},${Math.round(b.y - top)} ${Math.round(b.width)}x${Math.round(b.height)}`;
    };
    const r = document.querySelector(`li.ms-row[data-agent="${row}"]`)?.getBoundingClientRect();
    const h = head?.getBoundingClientRect();
    // The composer's To line: one line, however many are picked.
    const to = document.querySelector(".ms-to")?.getBoundingClientRect();
    return {
      row: r ? `${Math.round(r.x)},${Math.round(r.y + scrollY)}` : null,
      head: h ? `${Math.round(h.width)}x${Math.round(h.height)}` : null,
      to: to ? `${Math.round(to.width)}x${Math.round(to.height)}` : null,
      buttons: [...document.querySelectorAll(".ms-bulk > *")].map((b) => `${(b.textContent ?? "").trim()} ${inHead(b)}`),
    };
  }, row);
}

/** Where things are once nothing moves by itself any more (what the page loads after it shows has come in). */
async function settled(page: Page, row: string) {
  let last = "";
  await expect
    .poll(
      async () => {
        const now = JSON.stringify(await places(page, row));
        const same = now === last;
        last = now;
        return same;
      },
      { intervals: [400], timeout: 15_000 },
    )
    .toBe(true);
  return places(page, row);
}

test("picking agents moves nothing: the list stays where it is, the head keeps its one line and its buttons their places", async ({ page }, info) => {
  test.skip(REAL, "the mock's agents");
  await page.goto("/agents");
  await expect(page.locator("li.ms-row").first()).toBeVisible();
  const touch = await touchScreen(page);
  // By touch the line is there from Select on, before any agent is picked (and the head no taller for it).
  const watched = "fan-s3";
  const resting = await settled(page, watched);
  if (touch) await head(page).getByRole("button", { name: "Select", exact: true }).tap();
  const before = await settled(page, watched);
  if (touch) {
    expect(before.head, "starting to select changed the list's head").toBe(resting.head);
    expect(before.row, "starting to select moved the list").toBe(resting.row);
    expect(before.buttons.map((b) => b.split(" ")[0])).toEqual(["Actions", "Clear"]);
    await expect(head(page).getByRole("button", { name: "Actions" })).toBeDisabled();
  }

  // "Release prep": its goals running (read a moment after it's picked), on the kiln server.
  await pick(page, "fam-orch");
  // The composer turned to messaging it and the line came: the list didn't move, not even for a moment.
  const now = await places(page, watched);
  expect(now.row, "picking the first agent moved the list").toBe(before.row);
  expect(now.head, "picking the first agent changed the list's head").toBe(before.head);
  // Where its end comes to rest once it has faded in (by touch it was there already).
  const first = await settled(page, watched);
  expect(first.row).toBe(before.row);
  if (touch) expect(first.buttons).toEqual(before.buttons);
  await inMenu(page, async () => {
    await expect(action(page, "Pause")).toBeEnabled();
    await expect(action(page, "Move to…")).toBeDisabled();
  });
  expect(await settled(page, watched), "the goal stacks coming in moved something").toEqual(first);

  // Into two digits, agents at work and ready, on the server and on runners (none of those the last test ends).
  for (const id of ["att-ci", "demo-claude", "fan-lead", "demo-codex", "sub-busy", "sub-done", "sub-codex", "demo-long", "fan-sign"]) {
    await pick(page, id);
    expect(await places(page, watched), `picking ${id} moved something`).toEqual(first);
  }
  await inMenu(page, async () => {
    await expect(action(page, "Move to…")).toBeEnabled();
  });
  await nothingSideways(page);
  await page.evaluate(() => window.scrollTo(0, 0));
  await page.screenshot({ path: info.outputPath(`mission-bulk-steady-${info.project.name}.png`) });

  // Putting one back moves nothing either.
  await putBack(page, "att-ci");
  expect(await places(page, watched)).toEqual(first);
});

test("Clear, or putting back the last one picked, ends selecting", async ({ page }) => {
  test.skip(REAL, "the mock's agents");
  await page.goto("/agents");
  const touch = await touchScreen(page);
  await pick(page, "fam-orch", "att-ci");
  await expect(bulk(page, 2)).toBeVisible();
  await bulk(page, 2).getByRole("button", { name: "Clear" }).click();
  await expect(page.getByRole("group", { name: /^What to do with the/ })).toHaveCount(0);
  await expect(page.locator("li.ms-row[data-selected]")).toHaveCount(0);
  if (touch) await expect(head(page).getByRole("button", { name: "Select", exact: true })).toBeVisible();

  await pick(page, "att-ci");
  await putBack(page, "att-ci");
  await expect(page.getByRole("group", { name: /^What to do with the/ })).toHaveCount(0);
  if (touch) {
    await expect(head(page).getByRole("button", { name: "Select", exact: true })).toBeVisible();
    // Selecting over, the rows' rings are put away with it.
    await expect(page.locator(".ms-pick[data-on]")).toHaveCount(0);
  }
});

test("selected agents are stopped and moved at once, each as its own would be, and how it went opens to each", async ({ page }, info) => {
  test.skip(REAL, "the mock's agents");
  await page.goto("/agents");
  // "Fix the flaky retry test" works (Codex, on metal-ryzen-01); "Update the docs" is ready (Claude Code, on metal-ryzen-02).
  await pick(page, "fam-tests", "fam-docs");
  await expect(bulk(page, 2)).toBeVisible();
  await inMenu(page, async () => {
    await expect(action(page, "Move to…")).toBeEnabled();
    await expect(action(page, "End…")).toBeEnabled();
    // Neither has goals to pause or resume, nor has stopped, to restart or remove: those are there, turned off.
    for (const name of ["Resume", "Pause", "Restart", "Remove"]) await expect(action(page, name)).toBeDisabled();
    // Stopping their turns is for the one working.
    await expect(action(page, "Stop their turns")).toHaveAccessibleName(/1 of 2$/);
    await action(page, "Stop their turns").click();
  });
  await expect(said(page)).toContainText("Stopped the turns of 1 agent");
  const broadcasts = async () => ((await (await page.request.get("/api/mock/broadcasts")).json()) as { broadcasts: unknown[] }).broadcasts;
  await expect.poll(broadcasts).toContainEqual(expect.objectContaining({ to: { ids: ["fam-tests"] }, command: { type: "interrupt" } }));

  await inMenu(page, () => action(page, "Move to…").click());
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
  await inMenu(page, () => action(page, "End…").click());
  const end = page.getByRole("group", { name: "End the selected agents" });
  await expect(end).toContainText("End 2 agents? Their processes stop and they leave the board. Their changes stay on disk.");
  await expect(end.getByRole("checkbox")).toHaveCount(0);
  await nothingSideways(page);
  await page.screenshot({ path: info.outputPath(`mission-bulk-end-${info.project.name}.png`) });
  await end.getByRole("button", { name: "Cancel", exact: true }).click();
  await expect(end).toHaveCount(0);
  await expect(rowOf(page, "fam-tests")).toHaveCount(1);
});

test("a selected agent's goals are paused and resumed, and ending it offers the agents it started", async ({ page }) => {
  test.skip(REAL, "the mock's agents");
  await page.goto("/agents");
  // "Release prep" runs its goals, and started three agents that still run.
  await pick(page, "fam-orch");
  await expect(bulk(page, 1)).toBeVisible();
  await inMenu(page, () => action(page, "Pause").click());
  await expect(said(page)).toContainText("Paused the goals of 1 agent: no next goal starts until you resume them");
  await inMenu(page, () => action(page, "Resume").click());
  await expect(said(page)).toContainText("Resumed 1 agent");
  await inMenu(page, async () => {
    await expect(action(page, "Pause")).toBeEnabled();
    // It runs on the kiln server, where it stays.
    await expect(action(page, "Move to…")).toBeDisabled();
  });

  await inMenu(page, () => action(page, "End…").click());
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
  await inMenu(page, () => action(page, "End…").click());
  await page.getByRole("group", { name: "End the selected agents" }).getByRole("button", { name: "End 2", exact: true }).click();
  await expect(said(page)).toContainText("Ended 2 agents");
  await expect(page.locator('li.ms-row[data-agent="run-freeze"], li.ms-row[data-agent="run-bench"]')).toHaveCount(0);
  await expect(page.getByRole("group", { name: /^What to do with the/ })).toHaveCount(0);
});
