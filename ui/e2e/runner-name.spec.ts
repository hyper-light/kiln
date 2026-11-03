/**
 * A runner kiln knows by its machine id is shown by its name (docs/machines.md), in the production
 * build against the mock's Mac mini: the Runners table shows its label, never its id; its details
 * say the id under the name; it's renamed there, and the name the server keeps (lowercase, `-` for
 * what a name can't hold) shows in the details' head and the table; a name another runner has is
 * refused, said so, and what was typed stays to be put right. Nothing scrolls sideways, the
 * longest name a runner can have included. On a desktop and a phone.
 */
import { expect, test, type Page } from "@playwright/test";
import { MAC_MINI } from "../mock/data.ts";

/** Against a real kiln server (playwright.config.ts): these rename the mock's runner. */
const REAL = !!process.env.E2E_BASE_URL;

test.skip(REAL, "renames the mock's runner");

/** The name the mock's Mac mini starts with. */
const LABEL = "metal-mac-mini";

/** The runner's name as the mock starts it, whatever ran before (the other project, a test that failed). */
async function asStarted(page: Page) {
  expect((await page.request.post(`/api/agents/${MAC_MINI}/label`, { data: { label: LABEL } })).ok()).toBe(true);
}

const rows = (page: Page) => page.locator("tr[data-href]");
const details = (page: Page, name: string) => page.getByRole("region", { name: `${name} in depth` });
const nameForm = (page: Page, name: string) => details(page, name).getByRole("form", { name: `The name of ${name}` });
/** The name at the head of its details (their charts have headings of their own). */
const head = (page: Page, name: string) => details(page, name).getByRole("heading", { level: 3, name, exact: true });

/** Nothing on the page wider than it: not the page, the table (it scrolls on its own), nor what's in the runner's details. */
async function nothingSideways(page: Page) {
  const seen = await page.evaluate(() => {
    const client = document.documentElement.clientWidth;
    const table = document.querySelector(".table-wrap");
    return {
      page: document.documentElement.scrollWidth - client,
      table: table ? table.scrollWidth - table.clientWidth : 0,
      over: [...document.querySelectorAll(".runner-details, .runner-details *")]
        .filter((e) => e.getBoundingClientRect().right > client + 0.5)
        .map((e) => `${e.tagName.toLowerCase()}.${String(e.className)}`),
    };
  });
  expect(seen.page, "the page scrolls sideways").toBeLessThanOrEqual(0);
  expect(seen.table, "the table scrolls sideways").toBeLessThanOrEqual(0);
  expect(seen.over, "something in the runner's details is wider than the page").toEqual([]);
}

test("a runner known by its machine id shows by its name, and is renamed in its details", async ({ page }, info) => {
  await asStarted(page);
  const typed = `Studio Mac ${info.project.name}`;
  const kept = `studio-mac-${info.project.name}`;

  await page.goto("/runners");
  await expect(rows(page).filter({ hasText: LABEL })).toHaveCount(1);
  await expect(rows(page).filter({ hasText: MAC_MINI })).toHaveCount(0);

  // Its details: the name, and the machine id kiln knows it by under it.
  await rows(page).filter({ hasText: LABEL }).click();
  await expect(page).toHaveURL(new RegExp(`[?&]agent=${MAC_MINI}(&|$)`));
  await expect(head(page, LABEL)).toBeVisible();
  await expect(details(page, LABEL).getByText(`machine id ${MAC_MINI}`)).toBeVisible();
  const field = nameForm(page, LABEL).getByRole("textbox", { name: "Name" });
  await expect(field).toHaveValue(LABEL);
  await nothingSideways(page);
  await page.screenshot({ path: info.outputPath(`runner-name-before-${info.project.name}.png`) });

  // Renamed: shown as the server keeps it, in the details' head, their field and the table.
  await field.fill(typed);
  await nameForm(page, LABEL).getByRole("button", { name: "Save" }).click();
  await expect(page.getByRole("list", { name: "Notifications" })).toContainText(`Renamed ${LABEL} to ${kept}`);
  await expect(head(page, kept)).toBeVisible();
  await expect(details(page, kept).getByText(`machine id ${MAC_MINI}`)).toBeVisible();
  await expect(nameForm(page, kept).getByRole("textbox", { name: "Name" })).toHaveValue(kept);
  await expect(rows(page).filter({ hasText: kept })).toHaveCount(1);
  await expect(rows(page).filter({ hasText: LABEL })).toHaveCount(0);
  await expect(page).toHaveURL(new RegExp(`[?&]agent=${MAC_MINI}(&|$)`));

  // A name another runner has: refused, said why, and what was typed stays to be put right.
  await nameForm(page, kept).getByRole("textbox", { name: "Name" }).fill("metal-ryzen-02");
  await nameForm(page, kept).getByRole("button", { name: "Save" }).click();
  const refused = page.getByRole("alert").filter({ hasText: "Rename failed" });
  await expect(refused).toContainText("metal-ryzen-02 is another runner's name (metal-ryzen-02)");
  await expect(head(page, kept)).toBeVisible();
  await expect(nameForm(page, kept).getByRole("textbox", { name: "Name" })).toHaveValue("metal-ryzen-02");
  await expect(nameForm(page, kept).getByRole("button", { name: "Save" })).toBeEnabled();
  await expect(rows(page).filter({ hasText: kept })).toHaveCount(1);
  await expect(rows(page).filter({ hasText: "metal-ryzen-02" })).toHaveCount(1);
  await nothingSideways(page);
  await page.screenshot({ path: info.outputPath(`runner-name-refused-${info.project.name}.png`) });

  await asStarted(page);
});

test("the longest name a runner can have fits, in the table and its details", async ({ page }, info) => {
  // 63 characters, the most kiln keeps, without a `-` to break at.
  const long = `mac${"studio".repeat(10)}`;
  expect(long).toHaveLength(63);
  expect((await page.request.post(`/api/agents/${MAC_MINI}/label`, { data: { label: long } })).ok()).toBe(true);
  try {
    await page.goto(`/runners?agent=${MAC_MINI}`);
    await expect(head(page, long)).toBeVisible();
    await expect(details(page, long).getByText(`machine id ${MAC_MINI}`)).toBeVisible();
    await expect(rows(page).filter({ hasText: long })).toHaveCount(1);
    await nothingSideways(page);
    await page.screenshot({ path: info.outputPath(`runner-name-long-${info.project.name}.png`) });
  } finally {
    await asStarted(page);
  }
});
