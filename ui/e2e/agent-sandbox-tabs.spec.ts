// An agent's Sandbox (or Host) and Agents tabs (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, P2, P3,
// P5), against the mock: reached from the desktop's tab bar and the phone's tab strip (which scrolls
// sideways to them); a microVM session's sandbox from its live bars down to its plots, its network
// default deny; a host session's tab named Host, with none of what only a microVM has; and the
// agent's own definition first in its Agents tab, its variant a swatch and a name, never a letter.
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

test("a microVM session's Sandbox: now on top, what it is, its components, its network default deny, volumes, plots", async ({ page }) => {
  const said = errors(page);
  await page.goto("/agents/fam-orch");
  await expect(page.getByRole("textbox", { name: "Message to the agent", exact: true })).toBeEnabled({ timeout: 30_000 });
  await tab(page, "Sandbox");
  const body = page.locator(".hx-inspector-body").filter({ visible: true }).first();
  // Where we are now, first: its engine and live bars.
  await expect(body.getByRole("region", { name: "Now" })).toContainText("microVM");
  for (const m of ["CPU", "Memory", "Disk"]) await expect(body.getByRole("meter", { name: m })).toBeVisible();
  await expect(body.getByRole("button", { name: "Pause" })).toBeVisible();
  // What only a microVM has.
  await expect(body).toContainText("Hex box");
  await expect(body).toContainText("rust-dev v8");
  await expect(body).toContainText("GITHUB_TOKEN");
  // Its network: what's granted, then nothing else.
  await expect(body).toContainText("api.anthropic.com:443");
  await expect(body).toContainText("nothing else");
  // A member disconnected from a network, in place.
  const member = body.getByRole("button", { name: /^(Disconnect|Connect) (harness|agent|mcp) / }).first();
  const was = await member.getAttribute("aria-pressed");
  await member.click();
  await expect(member).not.toHaveAttribute("aria-pressed", was ?? "");
  await member.click();
  // Components: the harness points to kiln's own switch; another swaps in place.
  await expect(body.getByRole("button", { name: "Switch" })).toBeVisible();
  await expect(body.getByRole("button", { name: "Swap" }).first()).toBeVisible();
  // The last hour, at the bottom.
  await expect(body).toContainText("Last hour");
  expect(said, said.join("\n")).toEqual([]);
});

test("a host session's tab is Host: it says it runs on the host, with no network, pool, Hex box or secret names", async ({ page }) => {
  const said = errors(page);
  await page.goto("/agents/fam-docs");
  await expect(page.getByRole("textbox", { name: "Message to the agent", exact: true })).toBeEnabled({ timeout: 30_000 });
  await tab(page, "Host");
  const body = page.locator(".hx-inspector-body").filter({ visible: true }).first();
  await expect(body).toContainText("Runs on the host, without a microVM");
  await expect(body).toContainText("its owner's choice");
  await expect(body.getByRole("button", { name: "Restart" })).toBeVisible();
  // Absent, not greyed.
  await expect(body.getByRole("button", { name: "Pause" })).toHaveCount(0);
  // (Its last hour still plots its network traffic: what's absent is the Network section, its grants.)
  await expect(body.getByRole("button", { name: /^Network/ })).toHaveCount(0);
  for (const gone of ["Hex box", "Secrets", "nothing else", "Ingress", "Egress"]) await expect(body.getByText(gone, { exact: true })).toHaveCount(0);
  // Volumes on the host: allowed directories and scope.
  await expect(body).toContainText("Adds to the agent's allowed directories and this session's scope");
  expect(said, said.join("\n")).toEqual([]);
});

test("the Agents tab: this session's agent first, its variant a swatch and a name, then your agents opening in place", async ({ page }) => {
  const said = errors(page);
  await page.goto("/agents/rev-big");
  await expect(page.getByRole("textbox", { name: "Message to the agent", exact: true })).toBeEnabled({ timeout: 30_000 });
  await tab(page, "Agents");
  const body = page.locator(".hx-inspector-body").filter({ visible: true }).first();
  await expect(body).toContainText("This agent");
  await expect(body.getByRole("button", { name: "Restart on v4" })).toBeVisible();
  await expect(body).toContainText("2 vCPU · 4 GiB");
  // No variant reads as a letter or an id.
  await expect(body).not.toContainText("variant 5e1c");
  await expect(body.locator(".df-key, .ms-vkey")).toHaveCount(0);
  // Another of yours, opened in place: its groups, one line each.
  await body.getByRole("button", { name: "Open pr-triage" }).click();
  await expect(body.getByRole("button", { name: /^Definition/ }).first()).toBeVisible();
  await expect(body.getByRole("button", { name: /^Sessions/ }).first()).toBeVisible();
  await expect(body.getByRole("button", { name: /^Experiment/ }).first()).toBeVisible();
  expect(said, said.join("\n")).toEqual([]);
});
