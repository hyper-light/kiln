/**
 * A field keeps the focus while what it asks for comes: typing in a filter, a search applied with
 * Enter, typing beside a view that takes the focus as it opens. Fields were made anew as their own
 * words applied (keyed by them), hidden behind their page's loading shape while their filter's
 * results loaded, or had the focus taken back by a view focusing itself on every render. The same
 * element keeps the focus and every word typed. In the production build against the mock, on a
 * desktop and a phone.
 */
import { expect, test, type Locator, type Page } from "@playwright/test";

/** Against a real kiln server (playwright.config.ts): these read the mock's data. */
const REAL = !!process.env.E2E_BASE_URL;

/** Types into `field` as a person does, `between` after the first words; the same element has the focus and every word after. */
async function keepsFocus(page: Page, field: Locator, between: () => Promise<void>) {
  await expect(field).toBeVisible();
  const el = await field.elementHandle();
  expect(el, "no field").not.toBeNull();
  if (!el) return;
  await field.click();
  const was = await field.inputValue();
  await page.keyboard.type("svc", { delay: 80 });
  await between();
  expect(await el.evaluate((e) => e.isConnected), "the field was made anew").toBe(true);
  expect(await el.evaluate((e) => document.activeElement === e), "the field lost the focus").toBe(true);
  await page.keyboard.type(":a", { delay: 80 });
  expect(await el.evaluate((e) => document.activeElement === e), "the field lost the focus as more was typed").toBe(true);
  expect(await el.evaluate((e) => (e as HTMLInputElement).value)).toBe(`${was}svc:a`);
}

/** Observe's searches, applied with Enter: each field by its name. */
const SEARCHES: [string, { role: "searchbox" | "textbox"; name: string }][] = [
  ["/observe/logs", { role: "searchbox", name: "Search" }],
  ["/observe/events", { role: "searchbox", name: "Search" }],
  ["/observe/traces", { role: "searchbox", name: "Search" }],
  ["/observe/errors", { role: "searchbox", name: "Search" }],
  ["/observe/incidents", { role: "searchbox", name: "Search incidents" }],
  ["/observe/profiles", { role: "searchbox", name: "Search the uploads' tags" }],
  ["/observe/containers", { role: "searchbox", name: "Find containers" }],
  ["/observe/hosts", { role: "textbox", name: "Find hosts" }],
];

for (const [path, field] of SEARCHES) {
  test(`${path}: a search applied with Enter keeps its field, and the focus in it`, async ({ page }) => {
    test.skip(REAL, "the mock's data");
    await page.goto(path);
    const box = page.getByRole(field.role, { name: field.name, exact: true });
    await keepsFocus(page, box, async () => {
      await page.keyboard.press("Enter");
      await expect(page).toHaveURL(/[?&](q|query)=svc/);
      // What it asked for has come: the page has its results again.
      await page.waitForTimeout(800);
    });
  });
}

test("the deployments' filter keeps the focus as its apps load", async ({ page }) => {
  test.skip(REAL, "the mock's data");
  await page.goto("/deployments");
  // The list has come (its loading shape's filter is inert, but still matches by name).
  await expect(page.getByRole("checkbox", { name: "Select every app shown" })).toBeEnabled({ timeout: 15_000 });
  const box = page.getByRole("combobox", { name: "Filter apps" });
  await keepsFocus(page, box, async () => {
    // Past the moment it waits before asking the server, and the answer.
    await expect(page).toHaveURL(/[?&]q=svc/);
    await page.waitForTimeout(800);
  });
});

/** A workflow for the flow view (the mock keeps none): one job, its steps an action and a command. */
const WORKFLOW = [
  "name: ci",
  "on: push",
  "jobs:",
  "  test:",
  "    runs-on: linux",
  "    steps:",
  "      - name: Check out",
  "        uses: actions/checkout@v4",
  "      - name: Test",
  "        run: make test",
  "",
].join("\n");

test("Tab from a step's name to its command in the flow view keeps the focus, the name written", async ({ page }) => {
  test.skip(REAL, "the mock's data");
  const repo = "hyper-light/kiln";
  const path = ".github/workflows/ci.yml";
  await page.route(/\/api\/ci\/repos\/[^/]+\/labels$/, (r) => r.fulfill({ json: { labels: [] } }));
  await page.route(/\/api\/ci\/repos\/[^/]+\/workflows(\?.*)?$/, (r) =>
    r.fulfill({ json: { ref: "main", sha: "0123456789abcdef", workflows: [{ path, dialect: "github", name: "ci", runs: true, errors: 0, warnings: 0 }] } }),
  );
  await page.route(/\/api\/ci\/repos\/[^/]+\/workflows\/\.github\/workflows\/ci\.yml(\?.*)?$/, (r) =>
    r.fulfill({ json: { path, ref: "main", sha: "0123456789abcdef", source: WORKFLOW, diagnostics: [], outline: null, kind: "workflow" } }),
  );
  await page.goto(`/workflows?repo=${encodeURIComponent(repo)}&path=${encodeURIComponent(path)}`);
  await page.getByRole("group", { name: "View" }).getByRole("button", { name: "Flow" }).click();
  const steps = page.getByRole("list", { name: "test's steps" });
  // A step's button is its number and its name.
  await steps.getByRole("button", { name: /^2\s*Test$/ }).click();
  const name = page.getByRole("textbox", { name: "Name", exact: true });
  const run = page.getByRole("textbox", { name: "Run", exact: true });
  await expect(run).toHaveValue("make test");
  const field = await run.elementHandle();
  expect(field, "no Run field").not.toBeNull();
  if (!field) return;
  await name.click();
  await page.keyboard.type("s");
  await page.keyboard.press("Tab");
  // The name is written as its field is left, and the field Tab went to is the same one, focused.
  await expect(steps.getByRole("button", { name: /^2\s*Tests$/ })).toBeVisible();
  expect(await field.evaluate((e) => e.isConnected), "the step's fields were made anew").toBe(true);
  expect(await field.evaluate((e) => document.activeElement === e), "the command's field lost the focus").toBe(true);
  await page.keyboard.press("End");
  await page.keyboard.type(" -v");
  await expect(run).toHaveValue("make test -v");
});

test("typing in a review's search beside its whole file keeps the focus in the search", async ({ page }) => {
  test.skip(REAL, "the mock's data");
  await page.goto("/ship");
  const row = page.locator(".ship-row", { hasText: "pipeline: dynamic matrices" }).first();
  await row.locator(".ship-main").click();
  await expect(row.locator(".rv-s").first()).toBeVisible();
  await row.locator(".rv-ib[title='Open the whole file']").first().click();
  // Once its text is in (a busy machine's WebKit can take a while).
  await expect(row.locator(".rv-full")).toBeVisible({ timeout: 15_000 });
  await keepsFocus(page, row.locator(".rv-q input"), async () => {
    await page.waitForTimeout(800);
  });
});
