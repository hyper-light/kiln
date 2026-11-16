// An Agentfile's builds and install sources (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, P4.5, P4.6),
// against the mock: rust-dev v9's image on each runner (building step by step, queued, refused in
// Shards' words, or why a runner doesn't build), and each install source a row with its pin and the
// build secrets its fetch is given (by name, as its labels say), opened at its line and written again by the source picker;
// claude-rust built in place, its rows going queued → building → built while shown.
import { expect as expectNow, test, type Page } from "@playwright/test";

/** Each wait on what the mock answers gives it 15 s, as other specs' waits on the server do. */
const expect = expectNow.configure({ timeout: 15_000 });

/** How far the page scrolls sideways. */
const sideways = (page: Page) => page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);

/** Console errors as the page prints them: none is the bar. */
function errors(page: Page) {
  const said: string[] = [];
  page.on("console", (m) => {
    if (m.type() === "error") said.push(m.text());
  });
  return said;
}

async function open(page: Page, name: string) {
  await page.goto("/agents/agentfiles");
  await page.getByRole("list", { name: "Your Agentfiles" }).getByRole("link", { name, exact: true }).click();
  await expect(page.getByRole("heading", { level: 1, name })).toBeVisible();
}

test("P4.5: its image on each runner, building step by step, refused in Shards' words, and why a runner doesn't build", async ({ page }) => {
  const said = errors(page);
  await open(page, "rust-dev");
  const builds = page.getByRole("list", { name: "Its image on each runner" });
  await expect(builds.getByRole("listitem")).toHaveCount(5);
  await expect(
    builds
      .getByRole("listitem")
      .filter({ hasText: "metal-ryzen-01" })
      .getByRole("meter", { name: /^Step \d of 9$/ }),
  ).toBeVisible();
  await expect(builds.getByRole("listitem").filter({ hasText: "metal-mac-mini" })).toContainText("queued");
  await expect(builds.getByRole("listitem").filter({ hasText: "metal-ryzen-02" })).toContainText("host engine");
  await expect(builds.getByRole("listitem").filter({ hasText: "metal-pi-04" })).toContainText("no microVMs");
  const epyc = builds.getByRole("listitem").filter({ hasText: "metal-epyc-01" });
  await expect(epyc).toContainText("refused");
  await expect(epyc).not.toContainText("Shards refused the build");
  await epyc.getByRole("button", { name: "Why metal-epyc-01 refused it" }).click();
  await expect(epyc).toContainText("Shards refused the build at line 13 (HARNESS codex)");
  await expect(epyc.getByRole("button", { name: "Build again" })).toBeEnabled();
  // Its versions' images beside them: v8's, which sessions start from on three runners.
  const v8 = page
    .getByRole("region", { name: "Versions" })
    .getByRole("listitem")
    .filter({ has: page.locator(".af-name", { hasText: /^v8$/ }) });
  await expect(v8.locator(".afv-runners")).toHaveText("3");
  await expect(page.locator(".cm-tooltip, [role=dialog]")).toHaveCount(0);
  expect(await sideways(page)).toBe(0);
  expect(said).toEqual([]);
});

test("P4.6: each install source a row, opened at its line, and written again in Shards' syntax by the source picker", async ({ page }) => {
  const said = errors(page);
  await open(page, "rust-dev");
  const sources = page.getByRole("list", { name: "What it installs from" });
  await expect(sources.getByRole("listitem")).toHaveCount(6);
  const reviewer = sources.getByRole("listitem").filter({ hasText: "agent reviewer" });
  await expect(reviewer).toContainText("v4 → ");
  // Git over ssh signs in with an SSH agent: kiln gives a build secrets alone, so no key is named.
  await expect(reviewer.locator(".afs-cred")).toHaveText("ssh agent");
  await reviewer.getByRole("button", { name: /^AGENT reviewer: line \d+$/ }).click();
  await expect(page.locator(".af-editor .cm-activeLine")).toHaveText("AGENT reviewer FROM git@github.com:hyper-light/agents.git#v4:reviewer");

  // A Git source: a new ref, written into its line.
  await reviewer.getByRole("button", { name: "Change reviewer's source" }).click();
  const picker = page.getByRole("form", { name: "reviewer's source" });
  await expect(picker.getByLabel("Ref")).toHaveValue("v4:reviewer");
  await picker.getByLabel("Ref").fill("v5:reviewer");
  await picker.getByRole("button", { name: "Write line" }).click();
  await expect(picker).toHaveCount(0);
  await expect(page.locator(".af-editor .cm-content")).toContainText("AGENT reviewer FROM git@github.com:hyper-light/agents.git#v5:reviewer");
  await expect(page.getByRole("button", { name: "Save" })).toBeEnabled();

  // An https tarball is pinned by its SHA-256, or nothing is written.
  const claude = sources.getByRole("listitem").filter({ hasText: "harness claude" });
  await claude.getByRole("button", { name: "Change claude's source" }).click();
  const sha = page.getByRole("form", { name: "claude's source" });
  await sha.getByLabel("SHA-256").fill("abc");
  await expect(sha).toContainText("64 hex digits");
  await expect(sha.getByRole("button", { name: "Write line" })).toBeDisabled();
  await sha.getByLabel("SHA-256").fill("a".repeat(64));
  await sha.getByRole("button", { name: "Write line" }).click();
  await expect(page.locator(".af-editor .cm-content")).toContainText(`HARNESS claude --checksum=sha256:${"a".repeat(64)} FROM https://downloads.claude.ai/claude-code/2.1.311/claude-linux.tar.gz`);
  await expect(page.locator(".cm-tooltip, [role=dialog]")).toHaveCount(0);
  expect(await sideways(page)).toBe(0);
  expect(said).toEqual([]);
});

test("P4.5: Build in place: its runners queued at once, then building and built while shown", async ({ page }) => {
  const said = errors(page);
  await open(page, "claude-rust");
  const builds = page.getByRole("list", { name: "Its image on each runner" });
  const ryzen = builds.getByRole("listitem").filter({ hasText: "metal-ryzen-01" });
  await expect(ryzen).toContainText("built");
  await page.getByRole("button", { name: /^Build v\d+$/ }).click();
  await expect(ryzen).toContainText(/queued|\d\/9/, { timeout: 2000 });
  await expect(ryzen.getByRole("meter", { name: /^Step \d of 9$/ })).toBeVisible();
  await expect(ryzen).toContainText("built", { timeout: 60_000 });
  await expect(page.getByRole("region", { name: "Builds" }).locator(".afb-sum")).toContainText("3/3 built", { timeout: 60_000 });
  expect(said).toEqual([]);
});
