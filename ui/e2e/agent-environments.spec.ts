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

/** No page scrolls sideways. */
const sideways = (page: Page) => page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);

test("a session on bare metal has the Engine tab: Environments and Agents, no Sandboxes; this session's first and marked, a secret by its name", async ({ page }) => {
  const said = errors(page);
  const { body, list } = await engine(page);
  await expect(body.locator(".in-sec-toggle")).toHaveText([/^Environments/, /^Agents/]);
  // The machine's user's, then this session's, marked, then the others.
  const lines = list.locator(":scope > li.df-defrow");
  await expect(lines.first()).toContainText("Machine's user environment");
  await expect(lines.first()).not.toContainText("this session");
  await expect(lines.nth(1)).toContainText("release-tools");
  await expect(lines.nth(1)).toContainText("this session");
  await expect(lines.nth(1)).toContainText("User's · v3");
  // Its variables a line each: a secret by its name, a value as it is, the machine's variable it takes.
  const vars = list.getByRole("list", { name: "release-tools's variables" }).locator("li");
  await expect(vars).toHaveText([/^GH_TOKEN\s*secret\s*GH_TOKEN$/, /^RUST_LOG\s*value\s*info$/, /^CARGO_HOME\s*machine\s*\$CARGO_HOME$/]);
  await expect(list.getByRole("list", { name: "Sessions running with release-tools" })).toContainText("Update the docs");
  // Sorted and found: most variables first after this session's; Secrets chosen, only those that give one.
  await body.getByRole("combobox", { name: "Sort environments" }).selectOption("vars");
  await expect(lines.locator(".df-defname")).toHaveText(["Machine's user environment", "release-tools", "aws-agents", "clean-ci"]);
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
  await expect(form.getByRole("group", { name: "Variable 2" })).toContainText("set in login shell");
  // One of the person's own secrets, by name; moved up one.
  await form.getByRole("button", { name: "Add a variable" }).click();
  await form.getByRole("combobox", { name: "Where variable 3 comes from" }).selectOption("secret");
  await form.getByRole("combobox", { name: "Variable 3's secret" }).selectOption("GH_TOKEN");
  await form.getByRole("button", { name: "Move variable 3 up" }).click();
  await form.getByRole("button", { name: "Create the new environment" }).click();
  await expect(form).toHaveCount(0);
  const card = list.locator(":scope > li.df-defrow", { hasText: "e2e-env" });
  await expect(card).toContainText("Clean · v1");
  const tree = list.getByRole("list", { name: "e2e-env: variables and sessions" });
  await expect(tree.getByRole("list", { name: "e2e-env's variables" }).locator("li .ev-name")).toHaveText(["RUST_LOG", "GH_TOKEN", "CARGO_HOME"]);

  // Changed in place: its first variable taken out, saved as v2.
  await card.getByRole("button", { name: "Edit e2e-env" }).click();
  const edit = list.getByRole("listitem", { name: "Edit e2e-env" });
  await edit.getByRole("button", { name: "Remove variable 1" }).click();
  await edit.getByRole("button", { name: "Save e2e-env" }).click();
  await expect(card).toContainText("Clean · v2");
  await expect(tree.getByRole("list", { name: "e2e-env's variables" }).locator("li .ev-name")).toHaveText(["GH_TOKEN", "CARGO_HOME"]);

  // Applied to this session, asked again in place: it starts again on its conversation.
  await tree.getByRole("button", { name: "Apply to this session" }).click();
  await expect(tree.getByRole("group", { name: "Apply e2e-env to this session?" })).toContainText("Update the docs starts again on its conversation, with e2e-env v2.");
  const put = page.waitForRequest((r) => r.method() === "PUT" && r.url().endsWith("/api/harness/sessions/fam-docs/agentenv"));
  await tree.getByRole("button", { name: "Apply e2e-env to this session" }).click();
  expect((await put).postDataJSON()).toEqual({ agentenv: expectNow.stringMatching(/^env-/) });
  const lines = list.locator(":scope > li.df-defrow");
  await expect(lines.nth(1)).toContainText("e2e-env");
  await expect(lines.nth(1)).toContainText("this session");
  await expect(list.locator(":scope > li.df-defrow", { hasText: "release-tools" })).not.toContainText("this session");

  // Deleting it while this session runs with it: refused, in the server's words.
  await tree.getByRole("button", { name: "Delete", exact: true }).click();
  await tree.getByRole("button", { name: "Delete e2e-env" }).click();
  await expect(tree.getByRole("alert")).toHaveText("e2e-env can't be deleted: “Update the docs” runs with it. Put it on another environment first.");

  // Back on the machine's user's environment, then deleted.
  await lines.first().getByRole("button", { name: "Go back to it" }).click();
  await list.getByRole("button", { name: "Apply the machine's user's environment to this session" }).click();
  await expect(lines.first()).toContainText("this session");
  await tree.getByRole("button", { name: "Delete", exact: true }).click();
  await tree.getByRole("button", { name: "Delete e2e-env" }).click();
  await expect(list.locator(":scope > li.df-defrow", { hasText: "e2e-env" })).toHaveCount(0);
  expect(await sideways(page)).toBe(0);
  expect(said, said.join("\n")).toEqual([]);
});

test("the composer's Environment row: the machine's user's by default, one of the person's carried into the start as `agentenv`, and kept", async ({ page }) => {
  const said = errors(page);
  await page.request.post("/api/mock/agentenvs");
  await page.goto("/agents");
  await page.getByRole("button", { name: /^More options/ }).click();
  const pick = page.getByLabel("The environment it runs with");
  await expect(pick).toHaveValue("");
  await expect(pick.locator("option").first()).toHaveText("Machine's user environment");
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
