// An agent's Engine tab, the one tab agent chat gains for a session in a microVM (kiln plan
// 01a1cd8e862e7169a074db3d5fa33f3d, P2, P3, P5), against the mock: reached from the desktop's tab bar and
// the phone's tab strip (which scrolls sideways to it); a microVM session's sandbox, its line with its
// round controls, then its bars each with its last hour, what it is, its network default deny, its
// components; a session on bare metal, with no Engine tab and only its subagents in its Mission row's
// dropdown; and after its sandboxes the agent's own definition, its variant a swatch and a name, never
// a letter, then the agents, found by a filter.
import { expect as expectNow, test, type Page } from "@playwright/test";

const expect = expectNow.configure({ timeout: 15_000 });

/** Console errors as the page prints them: none is the bar. */
function errors(page: Page) {
  const said: string[] = [];
  page.on("console", (m) => {
    if (m.type() === "error") said.push(m.text());
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

test("a microVM session's Engine: its line and controls, its bars each with its last hour, what it is, its network default deny, its components", async ({ page }) => {
  const said = errors(page);
  await page.goto("/agents/fam-orch");
  await expect(page.getByRole("textbox", { name: "Message to the agent", exact: true })).toBeEnabled({ timeout: 30_000 });
  await tab(page, "Engine");
  const body = page.locator(".hx-inspector-body").filter({ visible: true }).first();
  // Its own sandbox first: its line, its round controls, its live bars and context.
  await expect(body.locator("li.df-vm").first()).toContainText("Release prep");
  await expect(body.getByRole("button", { name: "Pause Release prep" })).toBeVisible();
  for (const m of ["CPU", "Memory", "Disk", "Context"]) await expect(body.getByRole("meter", { name: m }).first()).toBeVisible();
  // What only a microVM has.
  await expect(body).toContainText("Hex box");
  await expect(body).toContainText("rust-dev v8");
  await expect(body).toContainText("GITHUB_TOKEN");
  // Its network: what's granted, then nothing else.
  await expect(body).toContainText("api.anthropic.com:443");
  await expect(body).toContainText("nothing else");
  // Egress granted live (the server's route), held in its state with how its microVM took it, then revoked.
  await body.getByRole("button", { name: "Grant egress" }).first().click();
  const grant = body.getByRole("listitem", { name: "Grant egress" });
  await grant.getByRole("textbox", { name: "Host: a DNS name or an address" }).fill("docs.rs");
  await grant.getByRole("button", { name: "Grant" }).click();
  const granted = body.getByRole("button", { name: "Revoke docs.rs:443" });
  await expect(granted).toBeVisible();
  await expect(granted.locator("xpath=ancestor::li[1]").locator(".dot")).toHaveAttribute("data-status", "succeeded");
  await granted.click();
  await expect(body.getByRole("button", { name: "Revoke docs.rs:443" })).toHaveCount(0);
  // Components: the harness's settings a row each under it (what its harness lets change: here its
  // permissions, a plain scale), its source, pin and path one line; another swaps in place.
  const harness = body.locator(".sb-comp").first();
  await expect(harness.getByRole("slider")).toHaveCount(0);
  await harness.getByRole("button", { name: /^Permissions:/ }).click();
  await expect(harness.getByRole("slider", { name: "Permissions" })).toBeVisible();
  await expect(harness.locator(".sb-src")).toContainText("/opt/kiln/harness/claude");
  await expect(body.getByRole("button", { name: "Switch" })).toHaveCount(0);
  await expect(body.getByRole("button", { name: "Swap" }).first()).toBeVisible();
  // Its network's last hour, under its bars; the tab's sections each fold (none nests in another).
  await expect(body.locator(".df-unet").first()).toContainText("Network");
  // Only its two sections fold; a card's sections are shown whole (no collapsible nested in another).
  await expect(body.locator(".in-sec-toggle")).toHaveText([/^Sandboxes/, /^Agents/]);
  for (const s of ["MicroVM", "Components", "Network", "Volumes"]) await expect(body.locator(".in-sec-static", { hasText: s }).first()).toBeVisible();
  expect(said, said.join("\n")).toEqual([]);
});

test("a session on bare metal: its Mission row's dropdown has its subagents only, and agent chat no Engine tab", async ({ page }) => {
  const said = errors(page);
  await page.goto("/agents?open=fam-docs");
  const row = page.locator('.ms-row[data-agent="fam-docs"]');
  // Where it runs is known (its row's engine mark): on its machine itself.
  await expect(row.locator(".sm-engine").first()).toHaveAttribute("data-engine", "host", { timeout: 30_000 });
  const groups = page.locator(".ms-dropdown .df-group .df-group-k");
  await expect(groups).toHaveText(["Subagents"]);
  await expect(page.locator(".ms-dropdown .df-note")).toHaveText(["No subagents."]);
  await row.getByRole("button", { name: /^Open / }).click();
  await expect(page.getByRole("textbox", { name: "Message to the agent", exact: true })).toBeEnabled({ timeout: 30_000 });
  await expect(
    page
      .getByRole("tab", { name: /^Tools/ })
      .filter({ visible: true })
      .first(),
  ).toBeVisible();
  await expect(page.getByRole("tab", { name: "Engine" })).toHaveCount(0);
  // One in a microVM started from no agent of yours: all its groups, its settings kiln's agent for its harness.
  await page.goto("/agents?open=demo-claude");
  await expect(groups).toHaveText(["Subagents", "Sandboxes", "Settings", "Experiments"]);
  await page.locator(".ms-dropdown .df-group", { hasText: "Settings" }).locator(".df-head-main").click();
  await expect(page.locator(".ms-dropdown .df-group", { hasText: "Settings" })).toContainText("harness-claude");
  expect(said, said.join("\n")).toEqual([]);
});

test("a harness's permissions, model and effort, a row each in its sandbox's components, each set from there", async ({ page }) => {
  const said = errors(page);
  await page.goto("/agents?open=demo-claude");
  const harness = page.locator(".ms-dropdown .sb-comp").first();
  // Each its label on a row, its value under it (the version and its source last), nothing open till a value is tapped.
  await expect(harness.locator(".sb-set > .sb-k")).toHaveText(["Permissions", "Model", "Effort", "Version"], { timeout: 30_000 });
  await expect(harness.getByRole("slider")).toHaveCount(0);
  // Effort: its plain scale, Home to its first stop.
  await harness.getByRole("button", { name: /^Effort:/ }).click();
  const effort = harness.getByRole("slider", { name: "Effort" });
  await effort.focus();
  await page.keyboard.press("Home");
  await expect(effort).toHaveAttribute("aria-valuetext", "Low");
  // The model by its family, its harness's mark after it.
  await harness.getByRole("button", { name: /^Model:/ }).click();
  await harness
    .getByRole("radio", { name: /^Sonnet/i })
    .first()
    .click();
  await expect(harness.locator('.sb-set[data-kind="model"] .sb-set-now')).toContainText("Sonnet");
  await expect(harness.locator('.sb-set[data-kind="model"] .sb-set-now .hx-mark')).toHaveText("CC");
  // A mode that turns checks off waits for "Turn them off".
  await harness.getByRole("button", { name: /^Permissions:/ }).click();
  const permissions = harness.getByRole("slider", { name: "Permissions" });
  await permissions.focus();
  await page.keyboard.press("End");
  await expect(harness.locator(".rs-confirm-line")).toBeVisible();
  await harness.getByRole("button", { name: "Turn them off" }).click();
  await expect(harness.locator(".rs-confirm-line")).toHaveCount(0);
  expect(said, said.join("\n")).toEqual([]);
});

test("after its sandboxes, the agents: this session's first, its settings open with what it started on, then each agent's card", async ({ page }) => {
  const said = errors(page);
  await page.goto("/agents/rev-big");
  await expect(page.getByRole("textbox", { name: "Message to the agent", exact: true })).toBeEnabled({ timeout: 30_000 });
  await tab(page, "Engine");
  const body = page.locator(".hx-inspector-body").filter({ visible: true }).first();
  const first = body.locator('[data-list="agents-tab"] > li.df-defrow').first();
  await expect(first).toContainText("reviewer");
  await expect(first).toContainText("this session");
  // Started on v3; the current one (whatever other tests saved since) is offered.
  await expect(body.getByRole("button", { name: /^Restart on v\d+$/ })).toBeVisible();
  await expect(body).toContainText("2 vCPU · 4 GiB");
  // No variant reads as a letter or an id.
  await expect(body).not.toContainText("variant 5e1c");
  await expect(body.locator(".df-key, .ms-vkey")).toHaveCount(0);
  // Every agent a card, the person's at first; its settings and experiments shown whole, nothing folding in it.
  const triage = body.locator('[aria-label="pr-triage: settings and experiments"]');
  await expect(triage.locator("li.df-head[data-fixed]")).toHaveText([/Settings/, /Experiments/]);
  await expect(triage.locator(".df-facts").first()).toBeVisible();
  await expect(triage.getByRole("button", { name: /^(Settings|Experiments)/ })).toHaveCount(0);
  expect(said, said.join("\n")).toEqual([]);
});
