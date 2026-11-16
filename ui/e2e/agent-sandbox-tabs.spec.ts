// An agent's Engine tab, the one tab agent chat gains (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, P2,
// P3, P5), against the mock: reached from the desktop's tab bar and the phone's tab strip (which
// scrolls sideways to it); a microVM session's sandbox, its line with its round controls, then its
// bars each with its last hour, what it is, its network default deny, its components; a host
// session's, with none of what only a microVM has; and after its engine the agent's own definition,
// its variant a swatch and a name, never a letter, then the agents, found by a filter.
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
  // Components: the harness points to kiln's own switch; another swaps in place.
  await expect(body.getByRole("button", { name: "Switch" })).toBeVisible();
  await expect(body.getByRole("button", { name: "Swap" }).first()).toBeVisible();
  // Its network's last hour, under its bars; no nested fold.
  await expect(body.locator(".df-unet").first()).toContainText("Network");
  await expect(body.locator(".in-sec-toggle")).toHaveCount(1);
  expect(said, said.join("\n")).toEqual([]);
});

test("a host session's Engine: on the host, with no network, pool, Hex box or secret names", async ({ page }) => {
  const said = errors(page);
  await page.goto("/agents/fam-docs");
  await expect(page.getByRole("textbox", { name: "Message to the agent", exact: true })).toBeEnabled({ timeout: 30_000 });
  await tab(page, "Engine");
  const body = page.locator(".hx-inspector-body").filter({ visible: true }).first();
  await expect(body.locator("li.df-vm .sm-engine").first()).toHaveAttribute("data-engine", "host");
  await expect(body).toContainText("its owner's choice");
  await expect(body.getByRole("button", { name: /^Restart / }).first()).toBeVisible();
  // Absent, not greyed.
  await expect(body.getByRole("button", { name: /^Pause / })).toHaveCount(0);
  // (Its last hour still plots its network traffic: what's absent is the Network section, its grants.)
  await expect(body.getByRole("button", { name: /^Network/ })).toHaveCount(0);
  for (const gone of ["Hex box", "Secrets", "nothing else", "Ingress", "Egress"]) await expect(body.getByText(gone, { exact: true })).toHaveCount(0);
  // Volumes on the host: allowed directories and scope; a folder given live, with how its harness and kiln's scope took it.
  await expect(body).toContainText("Adds to the agent's allowed directories and this session's scope");
  await body.getByRole("button", { name: "Add a volume" }).click();
  const form = body.getByRole("listitem", { name: "Add a volume" });
  await form.getByRole("textbox", { name: "Folder on its machine" }).fill("/data/fixtures");
  await form.getByRole("button", { name: "Add" }).click();
  const given = body.getByRole("button", { name: "Take /data/fixtures back" });
  await expect(given).toBeVisible();
  await expect(given.locator("xpath=ancestor::li[1]").locator(".sb-took .dot")).toHaveCount(2);
  await given.click();
  await body.getByRole("group", { name: "Take /data/fixtures back?" }).getByRole("button", { name: "Take /data/fixtures back" }).click();
  await expect(body.getByRole("button", { name: "Take /data/fixtures back" })).toHaveCount(0);
  expect(said, said.join("\n")).toEqual([]);
});

test("after its engine, this session's agent, its variant a swatch and a name, then the agents, found by a filter and opening in place", async ({ page }) => {
  const said = errors(page);
  await page.goto("/agents/rev-big");
  await expect(page.getByRole("textbox", { name: "Message to the agent", exact: true })).toBeEnabled({ timeout: 30_000 });
  await tab(page, "Engine");
  const body = page.locator(".hx-inspector-body").filter({ visible: true }).first();
  await expect(body).toContainText("This agent");
  // Started on v3; the current one (whatever other tests saved since) is offered.
  await expect(body.getByRole("button", { name: /^Restart on v\d+$/ })).toBeVisible();
  await expect(body).toContainText("2 vCPU · 4 GiB");
  // No variant reads as a letter or an id.
  await expect(body).not.toContainText("variant 5e1c");
  await expect(body.locator(".df-key, .ms-vkey")).toHaveCount(0);
  // The person's agents by default (a User chip in the filter), the system's by its facet.
  await expect(body.getByRole("button", { name: "Open pr-triage" })).toBeVisible();
  await expect(body.getByRole("button", { name: "Open kiln-ci-diagnose" })).toHaveCount(0);
  // Another of yours, opened in place: its groups, one line each.
  await body.getByRole("button", { name: "Open pr-triage" }).click();
  await expect(body.getByRole("button", { name: /^Settings/ }).first()).toBeVisible();
  await expect(body.getByRole("button", { name: /^Sessions/ }).first()).toBeVisible();
  await expect(body.getByRole("button", { name: /^Experiment/ }).first()).toBeVisible();
  expect(said, said.join("\n")).toEqual([]);
});
