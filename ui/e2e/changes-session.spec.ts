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
  // The head's total is what it shows.
  await expect(page.locator(".cx-total .hx-stat")).toHaveText(/\+7/);
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
