/**
 * The Changes tab's Session view (changes.tsx; the Changes panel plan 01a194444b1d, step 2), on the
 * mock's fam-orch: its files are its own, its two subagents', the person's elsewhere and the agents'
 * own. Scope shows this project's at first, then adds each band; By narrows to one who changed
 * them; the tab counts what the scope shows. Every column of the head and the list lines up in
 * every state, at a phone's width and a desktop's, and opening a menu moves nothing on the page.
 */
import { expect, test, type Page } from "@playwright/test";

/** Against a real kiln server (playwright.config.ts): these drive the mock's agents. */
const REAL = !!process.env.E2E_BASE_URL;

async function open(page: Page) {
  await page.goto("/agents/fam-orch?tab=changes");
  // A phone shows the page's own tabs; a desktop the inspector's, Changes first.
  const tab = page
    .getByRole("tab", { name: /^Changes/ })
    .filter({ visible: true })
    .first();
  await tab.click();
  await expect(page.locator(".cx-head")).toBeVisible();
}

const files = (page: Page) => page.locator(".cx-file:visible");
const bands = (page: Page) => page.locator(".cx-band:visible").allTextContents();
const tabCount = (page: Page) =>
  page
    .getByRole("tab", { name: /^Changes/ })
    .filter({ visible: true })
    .first()
    .innerText();

async function pick(page: Page, menu: "Scope" | "By", item: RegExp) {
  await page.getByRole("button", { name: new RegExp(`^${menu}:`) }).click();
  await page.getByRole("menuitemradio", { name: item }).click();
  await expect(page.getByRole("menu")).toBeHidden();
}

/** Each column's edges, from the panel's content edge: left edges for labels and values, right edges for totals, stats and ages. */
async function columns(page: Page) {
  return page.evaluate(() => {
    const head = document.querySelector(".cx-head");
    if (!head) return null;
    const x0 = head.getBoundingClientRect().left;
    // Where each shown one's content starts (or ends): inside its padding, where its words and icons are. An icon is SVG: it has no offsetParent, but boxes as any element does.
    const edge = (e: Element, side: "left" | "right") => {
      const r = e.getBoundingClientRect();
      const st = getComputedStyle(e);
      return side === "left" ? r.left + parseFloat(st.paddingLeft) + parseFloat(st.borderLeftWidth) : r.right - parseFloat(st.paddingRight) - parseFloat(st.borderRightWidth);
    };
    const edges = (sel: string, side: "left" | "right") => [...new Set([...document.querySelectorAll(sel)].filter((e) => e.getClientRects().length > 0).map((e) => Math.round(edge(e, side) - x0)))];
    return {
      labels: edges(".cx-lab, .cx-modes", "left"),
      values: edges(".cx-val > svg:first-child", "left"),
      names: edges(".cx-file .cx-path, .cx-band", "left"),
      right: edges(".cx-total .hx-stat, .cx-file > .hx-stat, .cx-when, .cx-view", "right"),
    };
  });
}

test("Scope shows this project's files at first, then each band; the tab counts what it shows", async ({ page }) => {
  test.skip(REAL, "the mock's agents");
  await open(page);
  await expect(page.getByRole("button", { name: "Scope: Project" })).toBeVisible();
  await expect(files(page)).toHaveCount(4);
  expect(await bands(page)).toEqual([]);
  expect(await tabCount(page)).toContain("4");
  await pick(page, "Scope", /^User/);
  await expect(files(page)).toHaveCount(6);
  expect(await bands(page)).toEqual(["Elsewhere"]);
  expect(await tabCount(page)).toContain("6");
  await pick(page, "Scope", /^All/);
  await expect(files(page)).toHaveCount(8);
  expect(await bands(page)).toEqual(["Elsewhere", "The agents' own"]);
  expect(await tabCount(page)).toContain("8");
  // Kept: the next visit shows the same.
  await page.reload();
  await open(page);
  await expect(page.getByRole("button", { name: "Scope: All" })).toBeVisible();
  await pick(page, "Scope", /^Project/);
});

test("By narrows to one who changed them; a file someone else changed too says so", async ({ page }) => {
  test.skip(REAL, "the mock's agents");
  await open(page);
  await pick(page, "Scope", /^All/);
  await pick(page, "By", /^Plan the planner goal/);
  await expect(page.getByRole("button", { name: "By: Plan the planner goal" })).toBeVisible();
  await expect(files(page)).toHaveCount(3);
  await expect(page.locator(".cx-file", { hasText: "mod.rs" }).locator(".cx-by")).toHaveText("also changed by Release prep");
  await expect(page.locator(".cx-file", { hasText: "plans.md" }).first().locator(".cx-by")).toHaveText("Plan the planner goal");
  // The head's total is what it shows: of mod.rs, which the agent changed too, only the planner's part (step 3).
  await expect(page.locator(".cx-total .hx-stat")).toHaveText(/\+6/);
  await pick(page, "By", /^Everyone/);
  await pick(page, "Scope", /^Project/);
  await expect(files(page)).toHaveCount(4);
});

test("every column lines up, at a phone's width and a desktop's, in every state", async ({ page }) => {
  test.skip(REAL, "the mock's agents");
  for (const width of [390, 1280]) {
    await page.setViewportSize({ width, height: 900 });
    await open(page);
    for (const [scope, by] of [
      [/^Project/, /^Everyone/],
      [/^All/, /^Everyone/],
      [/^All/, /^Plan the planner goal/],
    ] as const) {
      await pick(page, "Scope", scope);
      await pick(page, "By", by);
      const c = await columns(page);
      expect(c, `${width}px`).not.toBeNull();
      for (const [name, edges] of Object.entries(c ?? {})) expect(edges, `${width}px, ${scope} ${by}: ${name}`).toHaveLength(1);
    }
    await pick(page, "By", /^Everyone/);
    await pick(page, "Scope", /^Project/);
  }
});

test("opening and closing Scope, By and Sort moves nothing on the page", async ({ page }) => {
  test.skip(REAL, "the mock's agents");
  await open(page);
  const boxes = () =>
    page.evaluate(() =>
      [...document.querySelectorAll(".cx-mrow, .cx-hrow, .cx-file")].map((e) => {
        const r = e.getBoundingClientRect();
        return [Math.round(r.left), Math.round(r.top + window.scrollY), Math.round(r.width), Math.round(r.height)].join(",");
      }),
    );
  const before = await boxes();
  for (const name of [/^Scope:/, /^By:/, /^Sort:/]) {
    await page.getByRole("button", { name }).click();
    await expect(page.getByRole("menu")).toBeVisible();
    expect(await boxes(), `${name} open`).toEqual(before);
    await page.keyboard.press("Escape");
    await expect(page.getByRole("menu")).toBeHidden();
    expect(await boxes(), `${name} closed`).toEqual(before);
  }
});

/** Its added and removed lines, as the diff below the list shows them. */
const changed = (page: Page) =>
  page
    .locator(".hx-changes > .hx-stack .hx-row[data-kind=add] .hx-code, .hx-changes > .hx-stack .hx-row[data-kind=del] .hx-code")
    .evaluateAll((els) => els.map((e) => `${e.closest(".hx-row")?.getAttribute("data-kind") === "add" ? "+" : "-"}${e.textContent ?? ""}`));

test("narrowed to one who changed it, a file another changed too shows only their part, or their edits one by one", async ({ page }) => {
  test.skip(REAL, "the mock's agents");
  await open(page);
  // The planner added a line to mod.rs after the agent changed another: its part is its line alone.
  await pick(page, "By", /^Plan the planner goal/);
  const mod = page.locator(".cx-file", { hasText: "mod.rs" });
  // Asked of its session: the session's numbers, dimmed, till its answer comes.
  await expect(mod).toHaveAttribute("data-pending", "true");
  await expect(mod.locator(".hx-stat")).toHaveText("+1−0");
  await expect(mod).not.toHaveAttribute("data-pending");
  await expect(mod.locator(".cx-by")).toHaveText("also changed by Release prep");
  await mod.click();
  await expect.poll(() => changed(page)).toEqual(["+pub mod ready;"]);
  await mod.click();
  // The agent itself: its own line of mod.rs, and of quic.rs its one line after the optimizer's.
  await pick(page, "By", /^Release prep/);
  await expect(mod.locator(".hx-stat")).toHaveText("+1−1");
  const quic = page.locator(".cx-file", { hasText: "quic.rs" });
  await expect(quic.locator(".hx-stat")).toHaveText("+1−0");
  await quic.click();
  await expect.poll(() => changed(page)).toEqual(["+    send.finish()?;"]);
  await quic.click();
  // The optimizer's part of quic.rs is past what the session keeps: its edits one by one, from the conversation, and it says so.
  await pick(page, "By", /^Plan the API optimization goal/);
  await expect(quic.locator(".hx-stat")).toHaveText("+3−2");
  await quic.click();
  await expect(page.locator(".hx-changes > .hx-stack")).toContainText("Its edits one by one: this session keeps no more copies to compare them by (64 MiB at most).");
  await expect
    .poll(() => changed(page))
    .toEqual([
      "-    let stream = conn.open_bi().await?;",
      "-    stream.write_all(&req).await?;",
      "+    let (mut send, recv) = conn.open_bi().await?;",
      "+    send.write_all(&req).await?;",
      "+    read_answer(recv).await",
    ]);
  // Everyone: the session's change, whole.
  await quic.click();
  await pick(page, "By", /^Everyone/);
  await expect(quic.locator(".hx-stat")).toHaveText("+4−2");
});

test("By lists the agents it started; picking one shows its own changes, read from its own session", async ({ page }) => {
  test.skip(REAL, "the mock's agents");
  await open(page);
  await page.getByRole("button", { name: /^By:/ }).click();
  const menu = page.getByRole("menu");
  await expect(menu.getByText("Agents it started")).toBeVisible();
  await expect(menu.getByRole("menuitemradio", { name: /^(Fix the flaky retry test|Update the docs|Check the migration)/ })).toHaveCount(3);
  await menu.getByRole("menuitemradio", { name: /^Fix the flaky retry test/ }).click();
  await expect(page.getByRole("button", { name: "By: Fix the flaky retry test" })).toBeVisible();
  // Its own changeset, at the tab's scope: not the lead's.
  await expect(files(page)).toHaveCount(1);
  const client = page.locator(".cx-file", { hasText: "client.rs" });
  await expect(client.locator(".hx-stat")).toHaveText("+6−2");
  await expect(client.locator(".cx-by")).toHaveText("Fix the flaky retry test");
  await pick(page, "By", /^Everyone/);
  await expect(files(page)).toHaveCount(4);
});
