/**
 * A row's ⋯ menu in Mission control is put in the page when its button is first pressed (docs/ci.md
 * W26-R15: a list of agents doesn't carry a menu for every row), and opens, takes the focus, follows
 * the keys and closes exactly as a menu that was there from the start: in the production build
 * against the mock, with a mouse, the keyboard and by touch. Every test but the last holds of a menu
 * that was always in the page too; the last is what's different.
 */
import { expect, test, type Page } from "@playwright/test";
import { firstNotWaiting } from "./mission-list";

const LIVE = 'ul[data-list="live"]';

test.skip(!!process.env.E2E_BASE_URL, "the mock's agents");
test.beforeEach(async ({ page, request }) => {
  expect((await request.post("/api/mock/attention/reset")).ok()).toBe(true);
  await page.goto("/agents");
  await page.locator(`${LIVE} > li.ms-row`).first().waitFor();
});
test.afterEach(async ({ page, request }) => {
  await page.close();
  // Asked once more if the connection kept since the test began was closed under it meanwhile: the second goes on a new one.
  expect((await request.post("/api/mock/attention/reset", { maxRetries: 1 })).ok()).toBe(true);
});

/** The first row of the list that isn't waiting on you (`more`: and is this too), in the page (mission-list.ts), and its menu's button. */
async function firstRow(page: Page, more = "") {
  const row = page.locator(`${LIVE} > li.ms-row[data-agent="${await firstNotWaiting(page, more)}"]`);
  return { row, button: row.getByRole("button", { name: /^Actions for / }) };
}
const menu = (page: Page) => page.getByRole("menu");
/** What has the focus: a menu's item by its words, the menu itself, a row's menu button, or something else. */
const focused = (page: Page) =>
  page.evaluate(() => {
    const el = document.activeElement;
    if (!el) return "nothing";
    const role = el.getAttribute("role");
    if (role === "menuitem" || role === "menuitemcheckbox") return `item:${(el.textContent ?? "").trim()}`;
    if (role === "menu") return "menu";
    if (el.getAttribute("aria-haspopup") === "menu") return `button:${el.getAttribute("aria-label") ?? ""}`;
    return el.tagName.toLowerCase();
  });

test("W26-R15 a row's menu button reads as a closed menu's, and opens it at a press without moving: the menu takes the focus, Escape gives it back", async ({ page }) => {
  const { button } = await firstRow(page);
  const name = (await button.getAttribute("aria-label")) ?? "";
  await expect(button).toHaveAttribute("aria-haspopup", "menu");
  await expect(button).toHaveAttribute("aria-expanded", "false");
  await expect(button).toHaveAttribute("data-state", "closed");
  await expect(button).toHaveAttribute("type", "button");
  await expect(menu(page)).toHaveCount(0);
  // In view before it's pressed, so the press scrolls nothing; then where it is on the page, and how it's drawn there.
  await button.scrollIntoViewIfNeeded();
  const drawn = () =>
    button.evaluate((b) => {
      const box = b.getBoundingClientRect();
      const style = getComputedStyle(b);
      return { left: box.left, top: box.top + scrollY, width: box.width, height: box.height, radius: style.borderRadius, display: style.display, icon: b.innerHTML };
    });
  const was = await drawn();
  await button.click();
  await expect(menu(page)).toBeVisible();
  // The button where it was and as it was, now saying its menu is open and which it is.
  await expect(button).toHaveAttribute("aria-expanded", "true");
  await expect(button).toHaveAttribute("data-state", "open");
  expect(await button.getAttribute("aria-controls")).toBe(await menu(page).getAttribute("id"));
  expect(await menu(page).getAttribute("aria-labelledby")).toBe(await button.getAttribute("id"));
  expect(await drawn()).toEqual(was);
  // Opened with the pointer, the menu has the focus, none of its items yet; the keys go through them from the first.
  await expect.poll(() => focused(page)).toBe("menu");
  await page.keyboard.press("ArrowDown");
  await expect.poll(() => focused(page)).toBe("item:Rename…");
  await page.keyboard.press("Escape");
  await expect(menu(page)).toHaveCount(0);
  await expect(button).toHaveAttribute("aria-expanded", "false");
  await expect(button).toHaveAttribute("data-state", "closed");
  await expect.poll(() => focused(page)).toBe(`button:${name}`);
  expect(await drawn()).toEqual(was);
  // And again, now that it's there: the same.
  await button.click();
  await expect(menu(page)).toBeVisible();
  await expect.poll(() => focused(page)).toBe("menu");
  await page.keyboard.press("Escape");
  await expect(menu(page)).toHaveCount(0);
  await expect.poll(() => focused(page)).toBe(`button:${name}`);
});

for (const key of ["Enter", "Space", "ArrowDown"]) {
  test(`W26-R15 ${key} on a row's menu button opens its menu with the first item focused, the first time as every time after, and the page takes none of it`, async ({ page }) => {
    const { button } = await firstRow(page);
    const name = (await button.getAttribute("aria-label")) ?? "";
    for (const time of ["first", "second"]) {
      await button.focus();
      await expect.poll(() => focused(page), { message: `${time} time` }).toBe(`button:${name}`);
      const at = await page.evaluate(() => scrollY);
      await page.keyboard.press(key);
      await expect(menu(page), `${time} time`).toBeVisible();
      await expect.poll(() => focused(page), { message: `${time} time` }).toBe("item:Rename…");
      // The keys go on from there (the menu moves the focus a moment after the key).
      await page.keyboard.press("ArrowDown");
      await expect.poll(() => focused(page), { message: `${time} time` }).toMatch(/^item:(?!Rename…)/);
      await page.keyboard.press("Escape");
      await expect(menu(page), `${time} time`).toHaveCount(0);
      await expect.poll(() => focused(page), { message: `${time} time` }).toBe(`button:${name}`);
      // The page didn't take the key for itself: Space and the arrow scroll a page, Enter presses what has the focus.
      expect(await page.evaluate(() => scrollY), `${time} time`).toBe(at);
      await expect(page).toHaveURL(/\/agents$/);
    }
  });
}

test("W26-R15 an item chosen from a menu opened for the first time does what it says and gives the focus back to its button, and a press outside closes one", async ({ page }) => {
  const { row, button } = await firstRow(page);
  const name = (await button.getAttribute("aria-label")) ?? "";
  await button.click();
  await page.getByRole("menuitem", { name: "Pin", exact: true }).click();
  await expect(row.getByRole("img", { name: "Pinned" })).toBeVisible();
  await expect(menu(page)).toHaveCount(0);
  // Its button, where the row now is.
  await expect.poll(() => focused(page)).toBe(`button:${name}`);
  const other = page.locator(`${LIVE} > li.ms-row:not([data-stand="needs"])`).nth(1);
  await other.getByRole("button", { name: /^Actions for / }).click();
  await expect(menu(page)).toBeVisible();
  await page.locator(".ms-listhead").click({ position: { x: 5, y: 5 } });
  await expect(menu(page)).toHaveCount(0);
});

test("W26-R15 End agent… from a menu opened for the first time asks first, in a dialog that takes the focus; Cancel closes it and the agent stays", async ({ page }) => {
  // One at work: it can be ended (one that has ended is removed instead).
  const { row, button } = await firstRow(page, '[data-stand="working"]');
  for (const time of ["first", "second"]) {
    await button.click();
    await page.getByRole("menuitem", { name: "End agent…" }).click();
    const dialog = page.getByRole("dialog");
    await expect(dialog, `${time} time`).toBeVisible();
    await expect(dialog.getByRole("heading"), `${time} time`).toHaveText(/^End .+\?$/);
    await expect(menu(page), `${time} time`).toHaveCount(0);
    await expect.poll(() => dialog.evaluate((d) => d.contains(document.activeElement)), { message: `${time} time: the focus is in the dialog` }).toBe(true);
    await dialog.getByRole("button", { name: "Cancel" }).click();
    await expect(dialog, `${time} time`).toHaveCount(0);
    await expect(row, `${time} time`).toHaveCount(1);
  }
});

test("W26-R15 by touch a tap on a row's menu button opens its menu, the first time as after", async ({ page, isMobile }) => {
  test.skip(!isMobile, "a phone's touches");
  const { row, button } = await firstRow(page);
  await button.scrollIntoViewIfNeeded();
  for (const time of ["first", "second"]) {
    await button.tap();
    await expect(menu(page), `${time} time`).toBeVisible();
    await expect(button, `${time} time`).toHaveAttribute("aria-expanded", "true");
    // The row wasn't opened or picked by it.
    await expect(page).toHaveURL(/\/agents$/);
    await expect(row).not.toHaveAttribute("data-selected", "true");
    await page.keyboard.press("Escape");
    await expect(menu(page), `${time} time`).toHaveCount(0);
  }
  await button.tap();
  await page.getByRole("menuitem", { name: "Pin", exact: true }).tap();
  await expect(row.getByRole("img", { name: "Pinned" })).toBeVisible();
});

test("W26-R15 no row carries a menu of its own until its button is pressed: of the rows in the page, none, then the one", async ({ page }) => {
  const { button } = await firstRow(page);
  const rows = page.locator(`${LIVE} > li.ms-row`);
  const buttons = rows.locator('button[aria-haspopup="menu"]');
  expect(await buttons.count()).toBe(await rows.count());
  // A menu in the page names its button (an id, for the menu to be labelled by): none has one yet.
  expect(await buttons.evaluateAll((all) => all.filter((b) => b.id !== "").length)).toBe(0);
  await button.click();
  await expect(menu(page)).toBeVisible();
  expect(await buttons.evaluateAll((all) => all.filter((b) => b.id !== "").length)).toBe(1);
  await expect(button).toHaveAttribute("id", /.+/);
});
