// Environments (docs/agents.md, "Environments", "Scopes, stacks, and applied on its own", "In the Engine
// tab"; decided by the user, 2026-11-18 and 2026-11-19), against the mock (mock/agentenvs.ts): the Engine
// tab on a session on bare metal, its Environments and Agents and no Sandboxes, this session's stack above
// the cards (top first, those that joined on their own badged) and its cards first and marked, a secret by
// its name; one made, changed in place, applied on top of this session's stack (which starts again on its
// conversation), refused deletion while it runs with it, then taken out and deleted; the stack reordered
// with up and down and by dragging; a card's scope as chips beside its "Apply on its own" switch; the
// filter by chips, from this session's own; and Mission control's composer carrying a stack chosen in More
// options into the start as `agentenvs`, offering only those whose scope matches it.
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

/** This session's stack, as its rows read (a name, then "auto" where it joined on its own). */
const stackOf = (body: import("@playwright/test").Locator) => body.getByRole("list", { name: "This session's environments" }).getByRole("listitem");

/** Every environment shown, its scope's filter cleared. */
async function showAll(body: import("@playwright/test").Locator) {
  await press(body.getByRole("button", { name: "Show all", exact: true }));
}

/** No page scrolls sideways. */
const sideways = (page: Page) => page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);

test("a session on bare metal has the Engine tab: Environments and Agents, no Sandboxes; this session's stack first and marked, a secret by its name", async ({ page }) => {
  const said = errors(page);
  const { body, list } = await engine(page);
  await expect(body.locator(".in-sec-toggle")).toHaveText([/^Environments/, /^Agents/]);
  // This session's stack at the top, top first: the one it was given, then those that joined on their own, badged.
  await expect(stackOf(body)).toHaveText([/^release-tools$/, /^linux-cache\s*auto$/, /^plan-quiet\s*auto$/]);
  await expect(body.getByRole("button", { name: "Take linux-cache out" })).toBeDisabled();
  await expect(body.getByText("Takes these at its next start")).toHaveCount(0);
  // Their cards first, in its order, marked.
  const lines = list.locator(":scope > li.df-defrow");
  await expect(lines.locator(".df-defname").first()).toHaveText("release-tools");
  for (const i of [0, 1, 2]) await expect(lines.nth(i).getByRole("img", { name: "This session runs with it" })).toBeVisible();
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
  // The sessions running with it, a row each in a table: this one marked.
  const ran = list.getByRole("table", { name: "Sessions running with release-tools" });
  await expect(ran.getByRole("columnheader")).toHaveText(["Session", "Machine", "State"]);
  await expect(ran.getByRole("row").nth(1)).toContainText("Update the docs");
  await expect(ran.getByRole("row").nth(1).getByRole("img", { name: "This session runs with it" })).toBeVisible();
  await expect(ran.getByRole("img", { name: "This session runs with it" })).toHaveCount(1);
  await expect(ran.getByRole("row")).toHaveCount(4);
  // Sorted and found: this session's first, then those its scope filter shows (clean-ci is Codex's), most variables first.
  await body.getByRole("combobox", { name: "Sort environments" }).selectOption("vars");
  await expect(lines.locator(".df-defname")).toHaveText(["release-tools", "linux-cache", "plan-quiet", "aws-agents"]);
  await showAll(body);
  await expect(lines.locator(".df-defname")).toHaveText(["release-tools", "linux-cache", "plan-quiet", "aws-agents", "clean-ci"]);
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

  // Applied from its card, asked first: it goes on top of this session's stack, which restarts the session.
  const body = page.locator(".hx-inspector-body").filter({ visible: true }).first();
  await press(tree.getByRole("button", { name: "Apply e2e-env to this session" }));
  await expect(tree.getByRole("group", { name: "Apply e2e-env?" })).toHaveText(/Restart this session with e2e-env on top\?/);
  let put = page.waitForRequest((r) => r.method() === "PUT" && r.url().endsWith("/api/harness/sessions/fam-docs/agentenv"));
  await press(tree.getByRole("button", { name: "Restart with e2e-env on top" }));
  // Those that joined on their own at its foot aren't sent: they join again on their own.
  expect((await put).postDataJSON()).toEqual({ agentenvs: [expectNow.stringMatching(/^env-/), "env-release"] });
  await expect(stackOf(body)).toHaveText([/^e2e-env$/, /^release-tools$/, /^linux-cache\s*auto$/, /^plan-quiet\s*auto$/]);
  const lines = list.locator(":scope > li.df-defrow");
  await expect(lines.first()).toContainText("e2e-env");
  await expect(lines.first().getByRole("img", { name: "This session runs with it" })).toBeVisible();
  await expect(tree.getByRole("button", { name: "Apply e2e-env to this session" })).toHaveCount(0);

  // Deleting it while this session runs with it: refused, in the server's words.
  await press(tree.getByRole("button", { name: "Delete", exact: true }));
  await press(tree.getByRole("button", { name: "Delete e2e-env" }));
  await expect(tree.getByRole("alert")).toHaveText("e2e-env can't be deleted: “Update the docs” runs with it. Put it on another environment first.");

  // Taken out of the stack, saved and asked again, then deleted.
  await press(body.getByRole("button", { name: "Take e2e-env out" }));
  await press(body.getByRole("button", { name: "Save this session's environments" }));
  put = page.waitForRequest((r) => r.method() === "PUT" && r.url().endsWith("/api/harness/sessions/fam-docs/agentenv"));
  await press(body.getByRole("button", { name: "Restart with these" }));
  expect((await put).postDataJSON()).toEqual({ agentenvs: ["env-release"] });
  await expect(stackOf(body)).toHaveText([/^release-tools$/, /^linux-cache\s*auto$/, /^plan-quiet\s*auto$/]);
  await expect(list.locator(":scope > li.df-defrow", { hasText: "e2e-env" }).getByRole("img", { name: "This session runs with it" })).toHaveCount(0);
  await press(tree.getByRole("button", { name: "Delete", exact: true }));
  await press(tree.getByRole("button", { name: "Delete e2e-env" }));
  await expect(list.locator(":scope > li.df-defrow", { hasText: "e2e-env" })).toHaveCount(0);
  expect(await sideways(page)).toBe(0);
  expect(said, said.join("\n")).toEqual([]);
});

test("a card's head is edited in place: renamed, tagged and untagged, described; each saved at once over its version", async ({ page }) => {
  const said = errors(page);
  const { body, list } = await engine(page);
  await showAll(body);
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
  const { body, list } = await engine(page);
  await showAll(body);
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
  await showAll(body);
  await expect(list.getByRole("list", { name: "clean-ci: variables and sessions" })).toBeVisible();
  await expect(list.getByRole("button", { name: "Expand aws-agents" })).toBeVisible();
  // Opened, it's its form: no edit button to press first.
  await list.getByRole("button", { name: "Expand aws-agents" }).click();
  await expect(list.getByRole("listitem", { name: "aws-agents's variables" }).getByRole("textbox", { name: "Variable 1's name" })).toHaveValue("AWS_ACCESS_KEY_ID");
  await expect(list.getByRole("button", { name: /^Edit / })).toHaveCount(0);
  expect(said, said.join("\n")).toEqual([]);
});

test("the composer's Environment row: the machine's user's by default, a stack of the person's whose scope matches, ordered, carried into the start as `agentenvs`, and kept", async ({ page }) => {
  const said = errors(page);
  await page.request.post("/api/mock/agentenvs");
  await page.goto("/agents");
  await page.getByRole("button", { name: /^More options/ }).click();
  const pick = page.getByLabel("The environment it runs with");
  await expect(pick).toHaveValue("");
  await expect(pick.locator("option").first()).toHaveText("Machine environment (default)");
  // Offered: those whose scope matches a Claude Code session (clean-ci is Codex's on Linux).
  await expect(pick.locator("option")).not.toContainText(["clean-ci v1"]);
  await pick.selectOption({ label: "release-tools v3" });
  const stack = page.getByRole("list", { name: "The environments it runs with" }).getByRole("listitem");
  await expect(stack).toHaveText(["release-tools"]);
  // Another added under it, then moved up.
  const add = page.getByLabel("Add an environment");
  await expect(add.locator("option").first()).toHaveText("Add");
  await expect(add.locator("option")).not.toContainText(["release-tools v3"]);
  await add.selectOption({ label: "aws-agents v2" });
  await expect(stack).toHaveText(["release-tools", "aws-agents"]);
  await page.getByRole("button", { name: "Move aws-agents up" }).click();
  await expect(stack).toHaveText(["aws-agents", "release-tools"]);
  // Closed, More options says it.
  await page.getByRole("button", { name: /^More options/ }).click();
  await expect(page.locator(".mc-sum")).toContainText("Environment: aws-agents + release-tools");
  const start = page.locator(".mc-start");
  await expect(start).toHaveAttribute("aria-label", /, with aws-agents \+ release-tools$/);
  await page.getByRole("textbox", { name: "Its first message" }).fill(`Check the docs build (${Date.now()})`);
  const asked = page.waitForRequest((r) => r.method() === "POST" && new URL(r.url()).pathname === "/api/harness/agents");
  await start.click();
  expect((await asked).postDataJSON()).toMatchObject({ agentenvs: ["env-aws", "env-release"] });
  await expect(page).toHaveURL(/\/agents\/[0-9a-f]{32}$/);
  // Kept for the next start; one taken out goes.
  await page.goto("/agents");
  await page.getByRole("button", { name: /^More options/ }).click();
  await expect(stack).toHaveText(["aws-agents", "release-tools"]);
  await page.getByRole("button", { name: "Take aws-agents out" }).click();
  await expect(stack).toHaveText(["release-tools"]);
  expect(await sideways(page)).toBe(0);
  expect(said, said.join("\n")).toEqual([]);
});

test("a card's scope: chips in their own section, added by key and value, taken away; Everywhere where it has none; its Apply on its own switch joins this session's stack", async ({ page }) => {
  const said = errors(page);
  const { body, list } = await engine(page);
  const patched = () => page.waitForRequest((r) => r.method() === "PATCH" && r.url().includes("/api/agentenvs/"));
  await list.getByRole("button", { name: "Expand release-tools" }).click();
  await expect(list.getByRole("list", { name: "release-tools's scope" })).toContainText("Everywhere");
  await list.getByRole("button", { name: "Expand aws-agents" }).click();
  const scope = list.getByRole("list", { name: "aws-agents's scope" });
  await expect(scope.getByRole("listitem")).toHaveText(["harness: claude"]);
  // Added: its key from the menu, its value typed (suggested), Enter.
  await scope.getByRole("button", { name: "Add to aws-agents's scope" }).click();
  const key = scope.getByRole("combobox", { name: "aws-agents's scope: key" });
  await key.selectOption("os");
  // As wide as its key and its arrow, in every engine (not its longest option's width).
  expect((await key.boundingBox())!.width).toBeLessThan(50);
  let sent = patched();
  await scope.getByRole("combobox", { name: "aws-agents's scope: os" }).fill("linux");
  await scope.getByRole("combobox", { name: "aws-agents's scope: os" }).press("Enter");
  expect((await sent).postDataJSON()).toEqual({
    version: 2,
    scope: [
      { key: "os", value: "linux" },
      { key: "harness", value: "claude" },
    ],
  });
  await expect(scope.getByRole("listitem")).toHaveText(["os: linux", "harness: claude"]);
  sent = patched();
  await scope.getByRole("button", { name: "Remove harness: claude" }).click();
  expect((await sent).postDataJSON()).toEqual({ version: 3, scope: [{ key: "os", value: "linux" }] });
  // Applied on its own: it joins this session's stack under the one given by name (by name among those of as many chips), badged;
  // its harness takes it as it next starts.
  const auto = list.getByRole("switch", { name: "Apply aws-agents on its own" });
  await expect(auto).toHaveAttribute("aria-checked", "false");
  sent = patched();
  await auto.click();
  expect((await sent).postDataJSON()).toEqual({ version: 4, auto: true });
  await expect(auto).toHaveAttribute("aria-checked", "true");
  await expect(stackOf(body)).toHaveText([/^release-tools$/, /^aws-agents\s*auto$/, /^linux-cache\s*auto$/, /^plan-quiet\s*auto$/]);
  await expect(body.getByText("Takes these at its next start")).toBeVisible();
  expect(await sideways(page)).toBe(0);
  expect(said, said.join("\n")).toEqual([]);
});

test("the filter by chips starts from this session's own; a chip taken away widens it; Show all and This session", async ({ page }) => {
  const said = errors(page);
  const { body, list } = await engine(page);
  const filter = body.getByRole("list", { name: "Scope filter" });
  await expect(filter.getByRole("listitem")).toHaveText(["machine: metal-ryzen-02", "os: linux", "arch: amd64", "harness: claude", /^model: /, "permission: claude:plan", "effort: claude:high"]);
  const names = list.locator(":scope > li.df-defrow .df-defname");
  await expect(names).not.toContainText(["clean-ci"]);
  // Its harness taken away: Codex's clean-ci on Linux is shown too.
  await filter.getByRole("button", { name: "Remove harness: claude" }).click();
  await expect(names.filter({ hasText: "clean-ci" })).toHaveCount(1);
  // One added: Windows only shows those whose scope names no system, or Windows.
  await filter.getByRole("button", { name: "Add to Scope filter" }).click();
  await filter.getByRole("combobox", { name: "Scope filter: key" }).selectOption("os");
  await filter.getByRole("combobox", { name: "Scope filter: os" }).fill("windows");
  await filter.getByRole("combobox", { name: "Scope filter: os" }).press("Enter");
  await filter.getByRole("button", { name: "Remove os: linux" }).click();
  await expect(names.filter({ hasText: "clean-ci" })).toHaveCount(0);
  await showAll(body);
  await expect(filter.getByRole("listitem")).toHaveCount(0);
  await expect(names).toHaveCount(5);
  await press(body.getByRole("button", { name: "This session", exact: true }));
  await expect(filter.getByRole("listitem")).toHaveCount(7);
  await expect(names).toHaveCount(4);
  expect(said, said.join("\n")).toEqual([]);
});

test("this session's stack reordered: with up and down (× puts it back), then by dragging, saved with ✓ and asked again", async ({ page }) => {
  const said = errors(page);
  const { body } = await engine(page);
  const rows = stackOf(body);
  await expect(rows).toHaveText([/^release-tools$/, /^linux-cache\s*auto$/, /^plan-quiet\s*auto$/]);
  await expect(body.getByRole("button", { name: "Save this session's environments" })).toHaveCount(0);
  await press(body.getByRole("button", { name: "Move release-tools down" }));
  await expect(rows).toHaveText([/^linux-cache\s*auto$/, /^release-tools$/, /^plan-quiet\s*auto$/]);
  await press(body.getByRole("button", { name: "Discard changes to this session's environments" }));
  await expect(rows).toHaveText([/^release-tools$/, /^linux-cache\s*auto$/, /^plan-quiet\s*auto$/]);
  // plan-quiet dragged by its handle to the top.
  await rows.first().evaluate((el) => el.scrollIntoView({ block: "center" }));
  const grip = rows.nth(2).locator(".ev-grip");
  const from = (await grip.boundingBox())!;
  const to = (await rows.first().boundingBox())!;
  await page.mouse.move(from.x + from.width / 2, from.y + from.height / 2);
  await page.mouse.down();
  await page.mouse.move(from.x + from.width / 2, to.y + 4, { steps: 8 });
  await page.mouse.up();
  await expect(rows).toHaveText([/^plan-quiet\s*auto$/, /^release-tools$/, /^linux-cache\s*auto$/]);
  await press(body.getByRole("button", { name: "Save this session's environments" }));
  await expect(body.getByRole("group", { name: "Restart this session?" })).toHaveText(/Restart this session with these\?/);
  const put = page.waitForRequest((r) => r.method() === "PUT" && r.url().endsWith("/api/harness/sessions/fam-docs/agentenv"));
  await press(body.getByRole("button", { name: "Restart with these" }));
  // Moved above one given by name, it's given by name; the one left at the foot joins on its own still.
  expect((await put).postDataJSON()).toEqual({ agentenvs: ["env-plan", "env-release"] });
  await expect(rows).toHaveText([/^plan-quiet$/, /^release-tools$/, /^linux-cache\s*auto$/]);
  await expect(body.getByText("Takes these at its next start")).toHaveCount(0);
  expect(await sideways(page)).toBe(0);
  expect(said, said.join("\n")).toEqual([]);
});
