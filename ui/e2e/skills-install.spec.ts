/**
 * Skills kiln installs, in an agent's Tools tab (docs/agents.md, "Skills kiln installs"), on the
 * mock's fam-docs: what's installed for it, with each machine's state; Add looks at a link (pstack
 * in cursor/plugins' marketplace), a folder (a Python and a Go skill, with hooks and an MCP server
 * off until turned on), an upload and words written here, and installs what it found. A long list
 * turns a page at a time: its pager on the left edge the rows and Remove start at, where it stays
 * on the last page; a search goes back to the first.
 */
import { expect, test, type Page } from "@playwright/test";

const REAL = !!process.env.E2E_BASE_URL;
/** pstack's principle-* skills, in the mock's copy of its Claude Code plugin. */
const PRINCIPLES = 24;

async function open(page: Page, agent = "fam-docs") {
  await page.goto(`/agents/${agent}?tab=tools`);
  // Other agents' asks, as cards over the page's corner: not what's tested here.
  await page.addStyleTag({ content: ".toast-viewport,.rq-viewport{display:none!important}" });
  await page
    .getByRole("tab", { name: /^Tools/ })
    .filter({ visible: true })
    .first()
    .click();
  // The Skills fold remembers being open: opened once, it opens open, maybe a moment after the page.
  const skills = page.locator(".in-sec-toggle", { hasText: "Skills" }).filter({ visible: true }).first();
  const panel = page.locator(".sk-panel").filter({ visible: true }).first();
  await expect(async () => {
    if ((await skills.getAttribute("aria-expanded")) === "false") await skills.click();
    await expect(skills).toHaveAttribute("aria-expanded", "true");
    await expect(panel.locator(".sk-pkg").first()).toBeVisible({ timeout: 5000 });
  }).toPass({ timeout: 30_000 });
  return panel;
}

const left = (page: Page, sel: string) =>
  page
    .locator(sel)
    .filter({ visible: true })
    .first()
    .evaluate((e) => Math.round(e.getBoundingClientRect().left));
const top = (page: Page, sel: string) =>
  page
    .locator(sel)
    .filter({ visible: true })
    .first()
    .evaluate((e) => Math.round(e.getBoundingClientRect().top - (e.closest(".sk-pkg")?.getBoundingClientRect().top ?? 0)));

test.describe("skills kiln installs", () => {
  test.skip(REAL, "drives the mock's agents and sources");

  for (const agent of ["codex-kids", "skills-devin", "acp-kids", "agy-kids"]) {
    test(`installed skills can be added, read and run in ${agent}`, async ({ page, request }) => {
      const sent: Record<string, unknown>[] = [];
      page.on("websocket", (ws) => {
        if (!ws.url().includes(`/api/harness/sessions/${agent}/`)) return;
        ws.on("framesent", (frame) => {
          const value = JSON.parse(String(frame.payload)) as { t?: string; c?: Record<string, unknown> };
          if (value.t === "cmd" && value.c) sent.push(value.c);
        });
      });
      await request.post(`/api/mock/${agent}`);
      await page.route("**/api/skills?*", async (route) => {
        const response = await route.fetch();
        await route.fulfill({ response, json: { ...(await response.json()), loads: true, native_components: false } });
      });
      const panel = await open(page, agent);
      await expect(panel).not.toContainText("Installed packages currently load in Claude Code");
      await panel.getByRole("button", { name: "Add", exact: true }).click();
      await panel.locator('[data-from="write"]').click();
      const name = `portable-${agent}`;
      await panel.getByLabel("Name", { exact: true }).fill(name);
      await panel.getByLabel("Description").fill("Use to check the installed skill in this harness.");
      await panel.getByLabel("Body").fill("Read references/checklist.md and run scripts/check.py.");
      await panel.getByRole("button", { name: "Look" }).click();
      await panel.locator(".sk-preview").getByRole("button", { name: "Install", exact: true }).click();
      const pkg = panel.locator(".sk-pkg", { hasText: name });
      await pkg.locator(".sk-pkg-name").click();
      const skill = pkg.locator(".sk-skill").first();
      await skill.getByRole("button", { name: "Read", exact: true }).click();
      await expect(skill.locator(".sk-read")).toBeVisible();
      await skill.getByRole("button", { name: "Run", exact: true }).click();
      await skill.locator("input").fill("check this diff");
      await skill.locator("form").getByRole("button", { name: "Run", exact: true }).click();
      await expect.poll(() => sent.find((command) => command.type === "skill_run")).toMatchObject({ type: "skill_run", name: `${name}:${name}`, args: "check this diff" });
    });
  }

  test("a link's package installs, and its long list pages on the rows' edge", async ({ page }) => {
    const panel = await open(page);
    await expect(panel.locator(".sk-pkg", { hasText: "tstack" })).toContainText("gh missing on jupiter");
    await expect(panel.locator(".sk-pkg", { hasText: "mattpocock-skills" }).getByRole("button", { name: /^Update to 9c1e0aa/ })).toBeVisible();

    await panel.getByRole("button", { name: "Add" }).click();
    await expect(panel.getByRole("button", { name: "Add" })).toBeDisabled();
    await panel.getByLabel("Link").fill("https://github.com/cursor/plugins/tree/main/pstack");
    await panel.getByRole("button", { name: "Look" }).click();
    const preview = panel.locator(".sk-preview");
    await expect(preview).toContainText("pstack");
    await expect(preview).toContainText("cursor/plugins @ 8ecfb19");
    await expect(preview.locator(".sk-row", { hasText: "Skills" })).toContainText("58");
    await expect(preview.getByLabel("Hooks")).toBeDisabled();
    await preview.getByRole("button", { name: "Install" }).click();

    const pstack = panel.locator(".sk-pkg", { hasText: "pstack" });
    await expect(pstack).toBeVisible();
    await expect(panel.locator(".sk-add")).toHaveCount(0);
    await pstack.locator(".sk-pkg-name").click();
    const pager = pstack.getByRole("group", { name: "pstack skills" });
    await expect(pager.locator("span")).toHaveText("1–10 of 58");
    await expect(pager.getByRole("button", { name: "Previous" })).toBeDisabled();
    await expect(pstack.locator(".sk-skill")).toHaveCount(10);

    // Its pager starts where its rows and Remove start (Ada, 11-09: left-aligned with them).
    const rows = await left(page, ".sk-pkg-body .sk-skill");
    expect(await left(page, ".sk-pkg-body .pager")).toBe(rows);
    expect(await left(page, ".sk-pkg-body > div:last-child > button")).toBe(rows);

    // Its last page is as tall as a full one: the pager doesn't move.
    const at = await top(page, ".sk-pkg-body .pager");
    for (const range of ["11–20", "21–30", "31–40", "41–50", "51–58"]) {
      await pager.getByRole("button", { name: "Next" }).click();
      await expect(pager.locator("span")).toHaveText(`${range} of 58`);
    }
    await expect(pager.getByRole("button", { name: "Next" })).toBeDisabled();
    await expect(pstack.locator(".sk-skill")).toHaveCount(8);
    expect(await top(page, ".sk-pkg-body .pager")).toBe(at);

    // More a page, and back; a search goes back to the first page.
    await panel.getByRole("button", { name: "25 a page" }).click();
    await expect(pager.locator("span")).toHaveText("1–25 of 58");
    await panel.getByRole("button", { name: "10 a page" }).click();
    await pager.getByRole("button", { name: "Next" }).click();
    await expect(pager.locator("span")).toHaveText("11–20 of 58");
    await panel.getByRole("searchbox", { name: "Search skills" }).fill("principle");
    await expect(pager.locator("span")).toHaveText(`1–10 of ${PRINCIPLES}`);

    // Read in place, through its own words.
    await panel.getByRole("searchbox", { name: "Search skills" }).fill("architect");
    await pstack.locator(".sk-skill", { hasText: "/pstack:architect" }).getByRole("button", { name: "Read" }).click();
    await expect(pstack.locator(".sk-read")).toContainText("architect");
  });

  test("a folder's code shows its setup, and what runs on its own is off", async ({ page }) => {
    const panel = await open(page);
    await panel.getByRole("button", { name: "Add" }).click();
    await panel.locator('[data-from="folder"]').click();
    await panel.getByLabel("Folder").fill("~/skills/perf");
    await panel.getByRole("button", { name: "Look" }).click();
    const preview = panel.locator(".sk-preview");
    await expect(preview).toContainText("uv sync");
    await expect(preview).toContainText("go build -o bin/ ./...");
    await expect(preview.getByLabel("Hooks")).not.toBeChecked();
    await expect(preview.getByLabel("MCP")).not.toBeChecked();
    await expect(preview.getByLabel("Setup")).toBeChecked();
    await preview.getByLabel("For").selectOption("mine");
    await preview.getByRole("button", { name: "Install" }).click();
    const perf = panel.locator(".sk-pkg", { hasText: "perf" });
    await expect(perf).toContainText("All my agents");
    await expect(perf).toContainText("go missing on adas-macbook-pro-2", { timeout: 35_000 });
    const retried = page.waitForRequest((r) => r.method() === "PATCH" && r.url().includes("/api/skills/"));
    await perf.getByRole("button", { name: "Retry setup" }).click();
    expect((await retried).postDataJSON()).toEqual({ on: true });
  });

  test("uploads, written skills, and a link with nothing there", async ({ page }) => {
    const panel = await open(page);
    await panel.getByRole("button", { name: "Add" }).click();
    await panel.getByLabel("Link").fill("https://github.com/someone/nothing");
    await panel.getByRole("button", { name: "Look" }).click();
    await expect(panel.locator(".sk-none")).toContainText("no skills or plugin there");
    await panel.locator('[data-from="upload"]').click();
    await panel.locator('input[name="upload"]').setInputFiles({ name: "release-notes.md", mimeType: "text/markdown", buffer: Buffer.from("---\nname: release-notes\n---\n") });
    await expect(panel.locator(".sk-preview")).toContainText("release-notes");
    // A marketplace archive keeps its file for the second Look, after a plugin is picked.
    await panel.locator('input[name="upload"]').setInputFiles({ name: "marketplace.zip", mimeType: "application/zip", buffer: Buffer.from("mock archive") });
    await panel.getByLabel("Plugin", { exact: true }).selectOption("pstack");
    await panel.getByRole("button", { name: "Look" }).click();
    await expect(panel.locator(".sk-preview")).toContainText("pstack");
    await expect(panel.locator(".sk-preview")).toContainText("marketplace.zip");
    await panel.locator('[data-from="write"]').click();
    await panel.getByLabel("Name").fill("changelog");
    await panel.getByLabel("Description").fill("Use when cutting a release.");
    await panel.getByLabel("Body").fill("# Changelog\n\n1. Read the merged changes since the last tag.");
    await panel.getByRole("button", { name: "Look" }).click();
    await expect(panel.locator(".sk-preview")).toContainText("changelog");
    await panel.getByRole("button", { name: "Cancel" }).click();
    await expect(panel.locator(".sk-add")).toHaveCount(0);
  });
});
