// Agentfiles (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 31), against the mock: kiln's read in
// place and copied as yours; yours written, edited with Shards' lint, saved, its versions read and one
// made current again, renamed and deleted; and an agent started from one in Mission control, the
// choice kept for its harness in its project. Everything in the page's flow, nothing sideways.
import { expect, test, type Page } from "@playwright/test";

/** How far the page scrolls sideways. */
const sideways = (page: Page) => page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);

/** The line under the page's title: its version and when it changed. */
const meta = (page: Page) => page.locator("p.af-dim", { hasText: /^v\d+ · changed/ });

/** What the last press said, beside Save. */
const said = (page: Page) => page.locator(".af-actions [role=status]");

/** Types `text` at the end of the editor's last line, as a person would. */
async function typeAtEnd(page: Page, text: string) {
  await expect(page.locator(".af-editor .cm-content")).toBeVisible();
  await page.locator(".af-editor .cm-line").last().click();
  await page.keyboard.press("End");
  await page.keyboard.insertText(text);
}

test("step31: kiln's are read in place and copied as yours; yours is edited with Shards' lint, saved, made current again from a version, renamed and deleted", async ({ page }) => {
  await page.goto("/agents/agentfiles");
  await expect(page.getByRole("heading", { level: 1, name: "Agentfiles" })).toBeVisible();
  const claude = page.getByRole("list", { name: "kiln's Agentfiles" }).getByRole("listitem").filter({ hasText: "harness-claude" });
  await claude.getByRole("button", { name: "Read" }).click();
  await expect(page.getByLabel("harness-claude's Agentfile")).toContainText("HARNESS claude");
  await claude.getByRole("button", { name: "Copy as yours" }).click();

  // Its own page, as yours, under a name that isn't kiln's.
  await expect(page).toHaveURL(/\/agents\/agentfiles\/[0-9a-f]{32}$/);
  const title = page.getByRole("heading", { level: 1 });
  await expect(title).toHaveText(/^harness-claude-\d+$/);
  const name = (await title.textContent()) ?? "";
  await expect(meta(page)).toContainText("v1 · changed");
  await expect(page.locator(".af-said")).toHaveText("Not checked: this server has no Shards to check it with.");

  // A line Shards refuses: marked on its line, said under the editor, and nothing kept.
  await typeAtEnd(page, "\nFORM alpine");
  await expect(page.locator(".af-said")).toHaveText("1 error.");
  await expect(page.locator(".af-editor .cm-lintRange-error")).toHaveCount(1);
  await page.getByRole("button", { name: "Save" }).click();
  await expect(said(page)).toContainText("Shards refuses this Agentfile: dockerfile parse error on line");
  await expect(meta(page)).toContainText("v1 · changed");
  await page.getByRole("button", { name: "Discard changes" }).click();
  await expect(page.locator(".af-editor .cm-content")).not.toContainText("FORM alpine");

  // A change it takes: a new version.
  await typeAtEnd(page, "\nLABEL org.example.step=31");
  await page.getByRole("button", { name: "Save" }).click();
  await expect(said(page)).toHaveText("Kept as v2. Not checked: this server has no Shards to check it with.");
  await expect(meta(page)).toContainText("v2 · changed");

  // Its versions: the first read in place and made current again, as a new version, its text in the editor.
  const versions = page.getByRole("region", { name: "Versions" });
  await expect(versions.getByRole("listitem")).toHaveCount(2);
  const first = versions.getByRole("listitem").filter({ hasText: /^v1\b/ });
  await first.getByRole("button", { name: "Read" }).click();
  await expect(page.getByLabel("v1's text")).toContainText("HARNESS claude");
  await expect(page.getByLabel("v1's text")).not.toContainText("LABEL org.example.step=31");
  await first.getByRole("button", { name: "Make current" }).click();
  await expect(said(page)).toHaveText("v1's text is current again, as v3. Not checked: this server has no Shards to check it with.");
  await expect(meta(page)).toContainText("v3 · changed");
  await expect(versions.getByRole("listitem")).toHaveCount(3);
  await expect(page.locator(".af-editor .cm-content")).not.toContainText("LABEL org.example.step=31");

  // A new name alone: no new version.
  await page.getByLabel("Its name").fill(`${name}-mine`);
  await page.getByRole("button", { name: "Save" }).click();
  await expect(said(page)).toHaveText("Renamed: a new name alone is no new version.");
  await expect(title).toHaveText(`${name}-mine`);
  await expect(meta(page)).toContainText("v3 · changed");
  await expect(page.locator(".cm-tooltip, [role=dialog]")).toHaveCount(0);
  expect(await sideways(page)).toBe(0);

  // Deleted after a question asked in place: back at the list, and gone from it.
  await page.getByRole("button", { name: "Delete Agentfile" }).click();
  const ask = page.getByRole("group", { name: "Confirm deleting the Agentfile" });
  await expect(ask).toContainText(`Delete ${name}-mine and its 3 versions?`);
  await ask.getByRole("button", { name: "Delete", exact: true }).click();
  await expect(page).toHaveURL(/\/agents\/agentfiles$/);
  await expect(page.getByRole("region", { name: "Yours" })).not.toContainText(`${name}-mine`);
});

test("step31: a new one is written in place and kept once its name is an image's", async ({ page }) => {
  await page.goto("/agents/agentfiles");
  await page.getByRole("button", { name: "New Agentfile" }).click();
  const form = page.getByRole("group", { name: "A new Agentfile" });
  await expect(form.locator(".cm-content")).toContainText("FROM ubuntu:24.04");
  const keep = form.getByRole("button", { name: "Keep it" });
  await form.getByLabel("Its name").fill("Not An Image");
  await expect(keep).toBeDisabled();
  const name = `step31-${Date.now()}`;
  await form.getByLabel("Its name").fill(name);
  await keep.click();
  await expect(page).toHaveURL(/\/agents\/agentfiles\/[0-9a-f]{32}$/);
  await expect(page.getByRole("heading", { level: 1 })).toHaveText(name);
  await expect(meta(page)).toContainText("v1 · changed");
  expect(await sideways(page)).toBe(0);
  await page.getByRole("button", { name: "Delete Agentfile" }).click();
  await page.getByRole("group", { name: "Confirm deleting the Agentfile" }).getByRole("button", { name: "Delete", exact: true }).click();
  await expect(page).toHaveURL(/\/agents\/agentfiles$/);
});

test("step31: an agent starts from the Agentfile chosen in More options, refused where its machine starts no microVMs, and the choice is kept", async ({ page }) => {
  const mine = ((await (await page.request.get("/api/agentfiles")).json()) as { agentfiles: { id: string; name: string }[] }).agentfiles.find((a) => a.name === "claude-rust");
  expect(mine).toBeTruthy();
  await page.goto("/agents");
  await page.getByRole("button", { name: /^More options/ }).click();
  const pick = page.getByLabel("The Agentfile Claude Code's microVM is made from");
  await expect(pick).toHaveValue("");
  await expect(pick.locator("optgroup")).toHaveCount(2);
  await pick.selectOption({ label: "claude-rust" });
  // The server's own kiln starts none: said under the row.
  await expect(page.locator(".mc-opts .mc-note")).toContainText("kiln's server starts no microVMs itself");
  await page
    .locator(".mc-opt")
    .filter({ has: page.locator(".mc-opt-key", { hasText: /^Machine$/ }) })
    .locator("button.mc-pick")
    .click();
  await page
    .getByRole("radiogroup", { name: "Machine" })
    .getByRole("radio", { name: /metal-ryzen-02/ })
    .click();
  await expect(page.locator(".mc-opts .mc-note")).toHaveCount(0);
  const start = page.locator(".mc-start");
  await expect(start).toHaveAttribute("aria-label", "Start: Claude Code on metal-ryzen-02, in a microVM from claude-rust");
  expect(await sideways(page)).toBe(0);
  await start.click();
  await expect(page).toHaveURL(/\/agents\/[0-9a-f]{32}$/);
  const id = new URL(page.url()).pathname.split("/").pop();
  // The start named it as the microVM's image.
  await expect
    .poll(async () => ((await (await page.request.get("/api/harness/sessions")).json()) as { id: string; isolation?: { kind: string; image?: string } }[]).find((a) => a.id === id)?.isolation)
    .toEqual({ kind: "microvm", image: `agentfile:${mine?.id}` });
  // Kept for Claude Code there: the next start begins with it.
  await page.goto("/agents");
  await page.getByRole("button", { name: /^More options/ }).click();
  await expect(page.getByLabel("The Agentfile Claude Code's microVM is made from")).toHaveValue(`agentfile:${mine?.id}`);
  await page.getByLabel("The Agentfile Claude Code's microVM is made from").selectOption({ label: "None: a process on its machine" });
  await expect(page.locator(".mc-start")).toHaveAttribute("aria-label", /^Start: Claude Code on [^,]+$/);
});
