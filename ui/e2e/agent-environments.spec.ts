// Environments (docs/agents.md, "Environments", "In the Engine tab"; decided by the user, 2026-11-18),
// against the mock (mock/agentenvs.ts): the Engine tab on a session on bare metal, its Environments and
// Agents and no Sandboxes, this session's environment first and marked, a secret by its name; one made,
// changed in place, applied to this session (which starts again on its conversation), refused deletion
// while it runs with it, then put back on the machine's user's environment and deleted; and Mission
// control's composer carrying the environment chosen in More options into the start as `agentenv`.
import { expect as expectNow, test, type Page } from "@playwright/test";

const expect = expectNow.configure({ timeout: 15_000 });

/** Console errors as the page prints them: a refusal's own line about its status aside. */
function errors(page: Page) {
  const said: string[] = [];
  page.on("console", (m) => {
    if (m.type() === "error" && !/Failed to load resource: the server responded with a status of 409/.test(m.text())) said.push(m.text());
  });
  return said;
}

/** The tab, by its name, wherever this window shows tabs (the desktop's bar, the phone's strip), scrolled to and pressed. */
async function tab(page: Page, name: string) {
  const t = page
    .getByRole("tab", { name: new RegExp(`^${name}`) })
    .filter({ visible: true })
    .first();
  await t.scrollIntoViewIfNeeded();
  await t.click();
  await expect(t).toHaveAttribute("aria-selected", "true");
}

/** fam-docs ("Update the docs", on metal-ryzen-02's own processes) with the mock's environments as they start out: its Engine tab. */
async function engine(page: Page) {
  await page.request.post("/api/mock/agentenvs");
  await page.goto("/agents/fam-docs");
  await expect(page.getByRole("textbox", { name: "Message to the agent", exact: true })).toBeEnabled({ timeout: 30_000 });
  await tab(page, "Engine");
  const body = page.locator(".hx-inspector-body").filter({ visible: true }).first();
  return { body, list: body.getByRole("list", { name: "Environments", exact: true }) };
}

/** `b` clicked from the middle of the page: on a phone, the notifications stacked at its foot would take a click there. */
async function press(b: import("@playwright/test").Locator) {
  await b.evaluate((el) => el.scrollIntoView({ block: "center" }));
  await b.click();
}

/** No page scrolls sideways. */
const sideways = (page: Page) => page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);

test("a session on bare metal has the Engine tab: Environments and Agents, no Sandboxes; this session's first and marked, a secret by its name", async ({ page }) => {
  const said = errors(page);
  const { body, list } = await engine(page);
  await expect(body.locator(".in-sec-toggle")).toHaveText([/^Environments/, /^Agents/]);
  // This session's environment, chosen at the top; its card first, marked.
  await expect(body.getByLabel("This session's environment").locator("option:checked")).toHaveText("release-tools");
  await expect(body.getByLabel("This session's environment").locator("option").first()).toHaveText("Machine environment (default)");
  const lines = list.locator(":scope > li.df-defrow");
  await expect(lines.first()).toContainText("release-tools");
  await expect(lines.first()).toContainText("this session");
  await expect(lines.first()).toContainText("Machine base · v3");
  // Opened, its variables are its fields: a secret by its name (never a value), a value as it is, the machine's variable it takes.
  await list.getByRole("button", { name: "Expand release-tools" }).click();
  const form = list.getByRole("listitem", { name: "release-tools's variables" });
  for (const [i, name, from] of [
    [1, "GH_TOKEN", "secret"],
    [2, "RUST_LOG", "value"],
    [3, "CARGO_HOME", "machine"],
  ] as const) {
    await expect(form.getByRole("textbox", { name: `Variable ${i}'s name` })).toHaveValue(name);
    await expect(form.getByRole("combobox", { name: `Where variable ${i} comes from` })).toHaveValue(from);
  }
  await expect(form.getByRole("combobox", { name: "Variable 1's secret" })).toHaveValue("GH_TOKEN");
  await expect(form.getByRole("textbox", { name: "Variable 2's value" })).toHaveValue("info");
  // Nothing changed: nothing to save or discard.
  await expect(form.getByRole("button", { name: "Save release-tools" })).toBeDisabled();
  await expect(form.getByRole("button", { name: "Discard changes to release-tools" })).toBeDisabled();
  await expect(list.getByRole("list", { name: "Sessions running with release-tools" })).toContainText("Update the docs");
  // Sorted and found: most variables first after this session's; Secrets chosen, only those that give one.
  await body.getByRole("combobox", { name: "Sort environments" }).selectOption("vars");
  await expect(lines.locator(".df-defname")).toHaveText(["release-tools", "aws-agents", "clean-ci"]);
  expect(await sideways(page)).toBe(0);
  expect(said, said.join("\n")).toEqual([]);
});

test("one made, changed in place, applied to this session, refused deletion while it runs with it, then put back and deleted", async ({ page }) => {
  const said = errors(page);
  const { list } = await engine(page);
  await list.getByRole("button", { name: "New environment" }).click();
  const form = list.getByRole("listitem", { name: "New environment" });
  await form.getByRole("textbox", { name: "Environment's name" }).fill("e2e-env");
  await form.getByRole("group", { name: "Base" }).getByRole("button", { name: "Clean" }).click();
  // A value.
  await form.getByRole("button", { name: "Add a variable" }).click();
  await form.getByRole("textbox", { name: "Variable 1's name" }).fill("RUST_LOG");
  await form.getByRole("textbox", { name: "Variable 1's value" }).fill("debug");
  // One the machine's user has, picked from what it says, with where it's set.
  await form.getByRole("button", { name: "Add a variable" }).click();
  await form.getByRole("combobox", { name: "Where variable 2 comes from" }).selectOption("machine");
  await form.getByRole("combobox", { name: "Variable 2's machine variable" }).selectOption("CARGO_HOME");
  await expect(form.getByRole("textbox", { name: "Variable 2's name" })).toHaveValue("CARGO_HOME");
  await expect(form.getByRole("combobox", { name: "Variable 2's machine variable" }).locator("option:checked")).toHaveText("CARGO_HOME · login shell");
  // One of the person's own secrets, by name; moved up one.
  await form.getByRole("button", { name: "Add a variable" }).click();
  await form.getByRole("combobox", { name: "Where variable 3 comes from" }).selectOption("secret");
  await form.getByRole("combobox", { name: "Variable 3's secret" }).selectOption("GH_TOKEN");
  await form.getByRole("button", { name: "Move variable 3 up" }).click();
  await form.getByRole("button", { name: "Create the new environment" }).click();
  await expect(form).toHaveCount(0);
  const card = list.locator(":scope > li.df-defrow", { hasText: "e2e-env" });
  await expect(card).toContainText("Clean base · v1");
  await list.getByRole("button", { name: "Expand e2e-env" }).click();
  const tree = list.getByRole("list", { name: "e2e-env: variables and sessions" });
  const names = () => tree.getByRole("listitem", { name: "e2e-env's variables" }).getByRole("textbox", { name: /^Variable \d+'s name$/ });
  await expect(names()).toHaveCount(3);
  for (const [i, n] of ["RUST_LOG", "GH_TOKEN", "CARGO_HOME"].entries()) await expect(names().nth(i)).toHaveValue(n);

  // Changed where it's shown: a change undone with ×, then one saved with ✓ as v2.
  const edit = tree.getByRole("listitem", { name: "e2e-env's variables" });
  await edit.getByRole("button", { name: "Remove variable 1" }).click();
  await expect(names()).toHaveCount(2);
  await edit.getByRole("button", { name: "Discard changes to e2e-env" }).click();
  await expect(names()).toHaveCount(3);
  await edit.getByRole("button", { name: "Remove variable 1" }).click();
  await edit.getByRole("button", { name: "Save e2e-env" }).click();
  await expect(card).toContainText("Clean base · v2");
  await expect(names()).toHaveCount(2);
  await expect(names().nth(0)).toHaveValue("GH_TOKEN");
  await expect(names().nth(1)).toHaveValue("CARGO_HOME");

  // Chosen for this session from the dropdown, confirmed first: it restarts the session.
  const body = page.locator(".hx-inspector-body").filter({ visible: true }).first();
  const choose = body.getByLabel("This session's environment");
  await choose.selectOption({ label: "e2e-env" });
  await expect(body.getByRole("group", { name: "Restart this session?" })).toHaveText(/Restart this session with e2e-env\?/);
  const put = page.waitForRequest((r) => r.method() === "PUT" && r.url().endsWith("/api/harness/sessions/fam-docs/agentenv"));
  await body.getByRole("button", { name: "Restart with e2e-env" }).click();
  expect((await put).postDataJSON()).toEqual({ agentenv: expectNow.stringMatching(/^env-/) });
  const lines = list.locator(":scope > li.df-defrow");
  await expect(lines.first()).toContainText("e2e-env");
  await expect(lines.first()).toContainText("this session");
  await expect(list.locator(":scope > li.df-defrow", { hasText: "release-tools" })).not.toContainText("this session");

  // Deleting it while this session runs with it: refused, in the server's words.
  await press(tree.getByRole("button", { name: "Delete", exact: true }));
  await press(tree.getByRole("button", { name: "Delete e2e-env" }));
  await expect(tree.getByRole("alert")).toHaveText("e2e-env can't be deleted: “Update the docs” runs with it. Put it on another environment first.");

  // Back on the machine environment, then deleted.
  await choose.selectOption({ label: "Machine environment (default)" });
  await body.getByRole("button", { name: "Restart with the machine environment" }).click();
  await expect(choose.locator("option:checked")).toHaveText("Machine environment (default)");
  await expect(list.locator(":scope > li.df-defrow", { hasText: "e2e-env" })).not.toContainText("this session");
  await press(tree.getByRole("button", { name: "Delete", exact: true }));
  await press(tree.getByRole("button", { name: "Delete e2e-env" }));
  await expect(list.locator(":scope > li.df-defrow", { hasText: "e2e-env" })).toHaveCount(0);
  expect(await sideways(page)).toBe(0);
  expect(said, said.join("\n")).toEqual([]);
});

test("a card's head is edited in place: renamed, tagged and untagged, described; each saved at once over its version", async ({ page }) => {
  const said = errors(page);
  const { list } = await engine(page);
  const card = list.locator(":scope > li.df-defrow", { hasText: "clean-ci" });
  const patched = () => page.waitForRequest((r) => r.method() === "PATCH" && r.url().includes("/api/agentenvs/"));
  // Renamed: the name pressed is a field in its place (the card then holds no "clean-ci" text, so it's found in the list); Enter saves.
  await card.getByRole("button", { name: "Rename clean-ci" }).click();
  const name = list.getByRole("textbox", { name: "clean-ci's name" });
  await name.fill("ci-clean");
  let sent = patched();
  await name.press("Enter");
  expect((await sent).postDataJSON()).toEqual({ version: 1, name: "ci-clean" });
  const renamed = list.locator(":scope > li.df-defrow", { hasText: "ci-clean" });
  await expect(renamed).toContainText("Clean base · v2");
  // A tag added under the name (its card opened), then one taken away.
  await list.getByRole("button", { name: "Expand ci-clean" }).click();
  await list.getByRole("button", { name: "Add a tag to ci-clean" }).click();
  sent = patched();
  await list.getByRole("textbox", { name: "New tag for ci-clean" }).fill("nightly");
  await list.getByRole("textbox", { name: "New tag for ci-clean" }).press("Enter");
  expect((await sent).postDataJSON()).toEqual({ version: 2, tags: ["ci", "nightly"] });
  const tags = list.getByRole("list", { name: "ci-clean's tags" });
  await expect(tags).toContainText("nightly");
  sent = patched();
  await tags.getByRole("button", { name: "Remove tag ci" }).click();
  expect((await sent).postDataJSON()).toEqual({ version: 3, tags: ["nightly"] });
  await expect(tags.getByRole("listitem")).toHaveText([/^nightly/]);
  await expect(renamed).toContainText("Clean base · v4");
  // Described in place; Escape leaves it as it was.
  const tree = list.getByRole("list", { name: "ci-clean: variables and sessions" });
  await tree.getByRole("button", { name: "Describe ci-clean" }).click();
  await tree.getByRole("textbox", { name: "ci-clean's description" }).fill("throwaway");
  await tree.getByRole("textbox", { name: "ci-clean's description" }).press("Escape");
  await expect(tree.getByRole("button", { name: "Describe ci-clean" })).toHaveText("Like CI: only PATH");
  expect(await sideways(page)).toBe(0);
  expect(said, said.join("\n")).toEqual([]);
});

test("each environment starts folded to its head, opens to its form, and is kept as it was left", async ({ page }) => {
  const said = errors(page);
  const { list } = await engine(page);
  for (const n of ["release-tools", "clean-ci", "aws-agents"]) {
    await expect(list.getByRole("button", { name: `Expand ${n}` })).toHaveAttribute("aria-expanded", "false");
    await expect(list.getByRole("list", { name: `${n}: variables and sessions` })).toHaveCount(0);
  }
  await expect(list.locator(":scope > li.df-defrow", { hasText: "clean-ci" })).toContainText("Clean base · v1");
  await list.getByRole("button", { name: "Expand clean-ci" }).click();
  await expect(list.getByRole("button", { name: "Collapse clean-ci" })).toHaveAttribute("aria-expanded", "true");
  await expect(list.getByRole("list", { name: "clean-ci: variables and sessions" })).toBeVisible();
  // Kept as it was left.
  await page.reload();
  await tab(page, "Engine");
  await expect(list.getByRole("list", { name: "clean-ci: variables and sessions" })).toBeVisible();
  await expect(list.getByRole("button", { name: "Expand aws-agents" })).toBeVisible();
  // Opened, it's its form: no edit button to press first.
  await list.getByRole("button", { name: "Expand aws-agents" }).click();
  await expect(list.getByRole("listitem", { name: "aws-agents's variables" }).getByRole("textbox", { name: "Variable 1's name" })).toHaveValue("AWS_ACCESS_KEY_ID");
  await expect(list.getByRole("button", { name: /^Edit / })).toHaveCount(0);
  expect(said, said.join("\n")).toEqual([]);
});

test("the composer's Environment row: the machine's user's by default, one of the person's carried into the start as `agentenv`, and kept", async ({ page }) => {
  const said = errors(page);
  await page.request.post("/api/mock/agentenvs");
  await page.goto("/agents");
  await page.getByRole("button", { name: /^More options/ }).click();
  const pick = page.getByLabel("The environment it runs with");
  await expect(pick).toHaveValue("");
  await expect(pick.locator("option").first()).toHaveText("Machine environment (default)");
  await pick.selectOption({ label: "release-tools v3" });
  // Closed, More options says it.
  await page.getByRole("button", { name: /^More options/ }).click();
  await expect(page.locator(".mc-sum")).toContainText("Environment: release-tools");
  const start = page.locator(".mc-start");
  await expect(start).toHaveAttribute("aria-label", /, with release-tools$/);
  await page.getByRole("textbox", { name: "Its first message" }).fill(`Check the docs build (${Date.now()})`);
  const asked = page.waitForRequest((r) => r.method() === "POST" && new URL(r.url()).pathname === "/api/harness/agents");
  await start.click();
  expect((await asked).postDataJSON()).toMatchObject({ agentenv: "env-release" });
  await expect(page).toHaveURL(/\/agents\/[0-9a-f]{32}$/);
  // Kept for the next start.
  await page.goto("/agents");
  await page.getByRole("button", { name: /^More options/ }).click();
  await expect(page.getByLabel("The environment it runs with")).toHaveValue("env-release");
  expect(await sideways(page)).toBe(0);
  expect(said, said.join("\n")).toEqual([]);
});
