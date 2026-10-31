/**
 * Mission control's composers are the agent page's field (compose-field.tsx): what's typed is marked
 * as it will read (marks.tsx), in a copy drawn under the field, whose own text is clear. Starting an
 * agent, its Markdown; messaging agents, kiln's own commands too. In the production build against the
 * mock, on a desktop and a phone.
 */
import { expect, test, type Locator, type Page } from "@playwright/test";

/** Against a real kiln server (playwright.config.ts): these drive the mock's agents. */
const REAL = !!process.env.E2E_BASE_URL;

const touchScreen = (page: Page) => page.evaluate(() => matchMedia("(pointer: coarse)").matches);

/** Selects an agent's row as the page lets one (mission-bulk.spec.ts): by touch, Select, then a tap; with a mouse, its checkbox. */
async function pick(page: Page, id: string) {
  const row = page.locator(`li.ms-row[data-agent="${id}"]`);
  await expect(row).toBeVisible();
  if (await touchScreen(page)) {
    const select = page.locator(".ms-listhead").getByRole("button", { name: "Select", exact: true });
    if (await select.isVisible()) await select.tap();
    await row.locator(".ms-main").tap();
  } else await row.getByRole("checkbox", { name: /^Select / }).check();
  await expect(row).toHaveAttribute("data-selected", "true");
}

/**
 * The copy lies on the field, measured in one frame: its edges on the field's (within a sliver of a
 * pixel), its lines as tall, scrolled as far. How far the field has scrolled inside.
 */
async function onTheField(field: Locator) {
  const { at, copy } = await field.evaluate((f) => {
    const box = (e: Element) => {
      const r = e.getBoundingClientRect();
      return { left: r.left, top: r.top, right: r.right, bottom: r.bottom, lines: e.scrollHeight, scrolled: e.scrollTop };
    };
    const under = f.parentElement?.querySelector(".cp-mirror");
    return { at: box(f), copy: under ? box(under) : null };
  });
  expect(copy, "no copy under the field").not.toBeNull();
  if (!copy) return 0;
  for (const edge of ["left", "top", "right", "bottom"] as const) expect(Math.abs(copy[edge] - at[edge]), `the copy's ${edge} edge`).toBeLessThan(0.5);
  expect(copy.lines, "the copy's lines").toBe(at.lines);
  expect(Math.abs(copy.scrolled - at.scrolled), "the copy scrolled with the field").toBeLessThan(1);
  return at.scrolled;
}

test("Mission control's composers mark what's typed as an agent's composer does", async ({ page }, info) => {
  test.skip(REAL, "the mock's agents");
  await page.goto("/agents");

  // Starting an agent: its first message's Markdown, marked as it's typed.
  const start = page.getByRole("textbox", { name: "Its first message" });
  const said = "Share workflows *and* actions across repos, **each** in `.kiln/` with [the docs](https://kiln.dev/ci)";
  await start.fill(said);
  const copy = page.locator(".mc .cp-mirror");
  await expect(copy).toContainText(said);
  await expect(copy.locator(".md-i")).toHaveText("*and*");
  await expect(copy.locator(".md-i")).toHaveCSS("font-style", "italic");
  await expect(copy.locator(".md-b")).toHaveText("**each**");
  await expect(copy.locator(".md-code")).toHaveText("`.kiln/`");
  await expect(copy.locator(".md-a")).toHaveText("[the docs](https://kiln.dev/ci)");
  // What shows is the copy: the field's own text is clear, and the copy lies under it line for line,
  // scrolled with it as a long message scrolls inside.
  await expect(start).toHaveCSS("color", "rgba(0, 0, 0, 0)");
  await start.fill(`${said}\n\n> Then the third tab, **Pipelines**, beside Commits and Deployments.\n- one\n- two`);
  await onTheField(start);
  await page.locator("section.ms-msg").screenshot({ path: info.outputPath(`mission-composer-marks-start-${info.project.name}.png`) });
  const steps = Array.from({ length: 12 }, (_, i) => `- step ${i + 1}: its *own* check`).join("\n");
  await start.fill(`${said}\n${steps}`);
  await page.keyboard.type(", then **land**");
  expect(await onTheField(start), "the long message scrolls inside the field").toBeGreaterThan(0);

  // Messaging an agent: the same field, what's written kept, and kiln's own commands marked wherever they're typed.
  await start.fill(said);
  await pick(page, "fam-orch");
  const message = page.getByRole("textbox", { name: /^Message / });
  await expect(message).toHaveValue(said);
  const marked = page.locator(".ms-msg:not(.mc) .cp-mirror");
  await expect(marked.locator(".md-i")).toHaveText("*and*");
  await message.fill("/restart");
  await expect(marked.locator(".md-cmd")).toHaveText("/restart");
  // A path isn't a command; a command after a space is.
  await message.fill("Look in /tmp first, then /interrupt");
  await expect(marked.locator(".md-cmd")).toHaveText("/interrupt");
  await onTheField(message);
  await page.locator("section.ms-msg").screenshot({ path: info.outputPath(`mission-composer-marks-message-${info.project.name}.png`) });
});
