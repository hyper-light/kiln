/**
 * Mission control's lists are windowed (docs/ci.md W21-R02, src/lib/list-window.ts): only the rows near
 * the viewport are in the page, gaps keeping the room of the rest. A spec that reads rows wherever
 * they are in a list (one by its id, all of them in order, one gone or back) makes the browser's
 * window as tall as the page first, so the whole list shows and every row is in the page, at its
 * project's own width. What the window itself does, at a desktop's and a phone's own heights, is
 * mission-window.spec.ts's.
 */
import { expect, type Page } from "@playwright/test";

/** Mission control, the whole of its lists in the page: opened (`go`: the way there, by default its address), then as tall a window as the page is. */
export async function wholeList(page: Page, go: () => Promise<unknown> = () => page.goto("/agents")) {
  await go();
  await page.locator('ul[data-list="live"] > li.ms-row').first().waitFor();
  const size = page.viewportSize();
  // The page is as tall as its lists (their gaps keep the room of the rows that aren't mounted): that much window leaves nothing to scroll to.
  const height = await page.evaluate(() => document.documentElement.scrollHeight);
  if (size && height > size.height) await page.setViewportSize({ width: size.width, height });
  await expect(page.locator("ul[data-list] > li[data-gap]")).toHaveCount(0);
}

/**
 * The first row of the live list that isn't waiting on you (`more`: and matches this too), by its
 * agent's id. With the list windowed, the page is scrolled down, as someone looking for the row
 * would, until every row before it is in the page and so is it.
 */
export async function firstNotWaiting(page: Page, more = ""): Promise<string> {
  for (;;) {
    const id = await page.locator('ul[data-list="live"] > li').evaluateAll((lines, more) => {
      for (const line of lines) {
        // Rows that aren't in the page stand here: it may be one of them.
        if (line.hasAttribute("data-gap")) return null;
        if (line.matches(`.ms-row:not([data-stand="needs"])${more}`)) return line.getAttribute("data-agent");
      }
      return null;
    }, more);
    if (id) return id;
    const moved = await page.evaluate(() => {
      const was = scrollY;
      scrollBy(0, innerHeight / 2);
      return scrollY !== was;
    });
    expect(moved, "no such row in the list").toBe(true);
    // The list follows the page at its next frame.
    await page.evaluate(() => new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(done))));
  }
}
