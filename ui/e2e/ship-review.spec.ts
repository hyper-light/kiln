/**
 * A Ship item's dropdown owns its review (W26-R07, R08, R12, R13): open an item, Files and the
 * Conversation first; reply, resolve, change a kind, delete, apply a suggestion, open a whole
 * file, search; resolve a conflict and commit it. The mock (mock/review.ts) answers the
 * endpoints; Chromium and WebKit, desktop and phone.
 */
import { expect, test, type Page } from "@playwright/test";

const open = async (page: Page, name: string) => {
  await page.goto("/ship");
  const row = page.locator(".ship-row", { hasText: name }).first();
  await row.locator(".ship-main").click();
  await expect(row.locator(".rv-s").first()).toBeVisible();
  return row;
};

test("Files and the Conversation are the dropdown's first sections", async ({ page }) => {
  const row = await open(page, "pipeline: dynamic matrices");
  const secs = row.locator(".ship-open > section");
  await expect(secs.nth(0)).toHaveAttribute("aria-label", "Files");
  await expect(secs.nth(1)).toHaveAttribute("aria-label", "Conversation");
  // The item's diff is in one place, Files (Ada, 2026-10-27), and Files takes focus: Ship's Diff button brings it there.
  const files = row.getByRole("region", { name: "Files", exact: true });
  await expect(files).toHaveCount(1);
  await files.focus();
  await expect(files).toBeFocused();
  // The kinds row shows kim's blocking thread holding Approve, in the gate's words.
  await expect(row.locator(".rv-k[data-k='blocking']").first()).toBeVisible();
  const approve = row.locator(".rv-bar .rv-btn", { hasText: "Approve" });
  await expect(approve).toBeDisabled();
  await expect(row.locator(".rv-thread .rv-m", { hasText: "can fail on a bad expression" })).toBeVisible();
});

test("nothing in Files or the Conversation scrolls sideways but a comment's code, which keeps each line on one line, at every width (W21-AC01)", async ({ page }) => {
  const row = await open(page, "pipeline: dynamic matrices");
  await expect(row.locator(".md-code-body").first()).toBeVisible();
  // As ci/ui/layout.mjs asks of every page: no element whose content scrolls left and right. A
  // table too wide to fit at all is the agent chat's exception (harness.css's .hx-table), and a
  // comment's code the mockup's (Ship's mockups plan, step 12): its lines don't wrap, its box scrolls.
  const sideways = await row.locator(".rv-s").evaluateAll((sections) =>
    sections.flatMap((s) =>
      [s, ...s.querySelectorAll("*")]
        .filter((el) => !el.closest(".hx-table, [data-code-scroll]"))
        .filter((el) => {
          const cs = getComputedStyle(el);
          return (cs.overflowX === "auto" || cs.overflowX === "scroll") && el.clientWidth > 0 && el.scrollWidth > el.clientWidth + 1;
        })
        .map((el) => `${el.tagName.toLowerCase()}.${String(el.className).trim().split(/\s+/).slice(0, 2).join(".")} (${el.scrollWidth - el.clientWidth}px)`),
    ),
  );
  expect(sideways).toEqual([]);
  // Each line of a comment's code on one line: no line taller than the shortest.
  const heights = await row.locator(".rv-s .md-ln").evaluateAll((ls) => ls.map((l) => l.getBoundingClientRect().height));
  expect(heights.length).toBeGreaterThan(0);
  expect(Math.max(...heights) - Math.min(...heights)).toBeLessThanOrEqual(1);
});

test("reply, resolve, change a kind, delete", async ({ page }) => {
  // The mock keeps what each project writes, so each project's words are its own.
  const tag = test.info().project.name;
  const row = await open(page, "pipeline: dynamic matrices");
  // Reply on kim's thread.
  const thread = row.locator(".rv-thread").first();
  await thread.locator(".rv-btn", { hasText: "Reply" }).click();
  await thread.locator("textarea").fill(`I'll name the axis in the error (${tag}).`);
  await thread.locator(".cp-send").click();
  await expect(thread.locator(".rv-m", { hasText: `I'll name the axis in the error (${tag})` })).toBeVisible();
  // Change the thread's kind through the ⋯ menu (your own thread is needed: make one).
  const line = row.locator(".rv-ln").nth(3);
  await line.click();
  const composer = row.locator(".rv-thread .rvc").last();
  await composer.locator(".rv-kind[data-k='nit']").click();
  await composer.locator("textarea").fill(`a nit of mine (${tag})`);
  await composer.locator(".cp-send").click();
  const mine = row.locator(".rv-thread", { hasText: `a nit of mine (${tag})` });
  await expect(mine.locator(".rv-k[data-k='nit']")).toBeVisible();
  // ⋯: copy, quote, edit, delete, kind. Delete it.
  await mine.locator(".rv-dots").first().click();
  await mine.locator(".rv-menu .rv-ib[aria-label='Delete']").click();
  await mine.locator(".rv-btn", { hasText: "Delete" }).click();
  await expect(row.locator(".rv-thread", { hasText: `a nit of mine (${tag})` })).toHaveCount(0);
  // Resolve kim's thread is disabled (blocking is its author's), then kind change through ⋯ on your own comment is shown.
  const resolve = thread.locator(".rv-btn", { hasText: "Resolve" });
  await expect(resolve).toBeDisabled();
});

test("apply a suggestion makes a revision", async ({ page }) => {
  // The mock keeps what each project writes, so the suggestion this applies is its own.
  const tag = test.info().project.name;
  const row = await open(page, "pipeline: dynamic matrices");
  const thread = row.locator(".rv-thread").first();
  await thread.locator(".rv-btn", { hasText: "Reply" }).click();
  await thread.locator("textarea").fill(`This, then (${tag}):\n\n\`\`\`suggestion\n        let axes = self.resolve(ctx).map_err(|e| e.legged())?;\n\`\`\``);
  await thread.locator(".cp-send").click();
  const mine = thread.locator(".rv-m", { hasText: `(${tag})` });
  await expect(mine.locator(".md-sug")).toBeVisible();
  await mine.locator(".md-apply").click();
  await expect(mine.locator(".md-applied")).toBeVisible();
});

test("the whole file opens in place (no popup)", async ({ page }) => {
  const row = await open(page, "pipeline: dynamic matrices");
  await row.locator(".rv-ib[title='Open the whole file']").first().click();
  const full = row.locator(".rv-full");
  await expect(full).toBeVisible();
  await expect(full.locator(".rv-l").first()).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(full).toHaveCount(0);
});

test("the vorpal search filters and jumps", async ({ page }) => {
  const row = await open(page, "pipeline: dynamic matrices");
  await row.locator(".rv-q input").fill("expand");
  await expect(row.locator(".rv-hits .rv-hit").first()).toBeVisible();
  await expect(row.locator(".rv-l.hit").first()).toBeVisible();
  await expect(row.locator(".rv-l[data-dim]").first()).toBeVisible();
  await row.locator(".rv-hits .rv-hit").first().click();
});

test("a conflict resolves where it falls and commits", async ({ page }) => {
  await page.request.post("/api/mock/review", { data: { conflicts: true } });
  const row = await open(page, "pipeline: dynamic matrices");
  const cf = row.locator(".rv-cf").first();
  await expect(cf).toBeVisible();
  await cf.getByRole("button", { name: "Take D41" }).click();
  await expect(cf).toHaveAttribute("data-done", "1");
  const second = row.locator(".rv-cf").nth(1);
  await second.getByRole("button", { name: "Take main" }).click();
  const commit = row.locator(".rv-btn", { hasText: "Commit resolution" });
  await expect(commit).toBeEnabled();
  await commit.click();
  await page.request.post("/api/mock/review", { data: { conflicts: false } });
});

test("every way to resolve a conflict, then Commit resolution clears the gate (W26-R12, AC04)", async ({ page }) => {
  await page.request.post("/api/mock/review", { data: { conflicts: true } });
  const row = await open(page, "pipeline: dynamic matrices");
  const commit = row.locator(".rv-btn", { hasText: "Commit resolution" });
  // While a conflict is unresolved, the gate holds Approve; once all are taken, Commit resolution stands in its place (the mock's conflictHTML).
  await expect(row.getByRole("button", { name: /^Approve/ })).toBeDisabled();
  await expect(commit).toHaveCount(0);
  // A text conflict at its lines: each choice resolves it through the API, and Undo takes it back.
  const text = row.locator(".rv-cf").first();
  // The mock keeps what another project took: start from nothing taken.
  for (const cf of [text, row.locator(".rv-cf").nth(1)]) if ((await cf.getAttribute("data-done")) === "1") await cf.getByRole("button", { name: "Undo" }).click();
  await expect(text).toContainText("conflict 1/2");
  await expect(text).toContainText("this change");
  await expect(text).toContainText("a person takes it");
  for (const take of ["Take main", "Take D41", "Both", "Use the agent's"]) {
    await text.getByRole("button", { name: take, exact: true }).click();
    await expect(text).toHaveAttribute("data-done", "1");
    await text.getByRole("button", { name: "Undo" }).click();
    await expect(text).not.toHaveAttribute("data-done", "1");
  }
  await text.getByRole("button", { name: "Edit" }).click();
  await text.getByRole("textbox", { name: "The resolution" }).fill("use serde_json::{Value, json};");
  await text.locator(".rv-btn-go").click();
  await expect(text).toHaveAttribute("data-done", "1");
  // A binary file's conflict is named, never markers, and resolved by taking a side.
  const binary = row.locator(".rv-cf").nth(1);
  await expect(binary).toContainText("binary file changed on both sides");
  await expect(binary).not.toContainText("<<<<<<<");
  await expect(binary.getByRole("button", { name: "Both", exact: true })).toHaveCount(0);
  await binary.getByRole("button", { name: "Take D41" }).click();
  await expect(binary).toHaveAttribute("data-done", "1");
  await expect(row.getByRole("button", { name: /^Approve/ })).toHaveCount(0);
  // The commit makes the revision; the conflicts go and Approve is back in its place.
  await commit.click();
  await expect(commit).toHaveCount(0);
  await expect(row.locator(".rv-cf")).toHaveCount(0);
  await expect(row.getByRole("button", { name: /^Approve/ })).toBeVisible();
});

test("the conflict ladder: a rung's line at one height, its tested resolution, Use the agent's, then Accept resolutions (W26-R17)", async ({ page }) => {
  await page.request.post("/api/mock/review", { data: { conflicts: true, ladder: "held", ladder_tests: "pending" } });
  const row = await open(page, "pipeline: dynamic matrices");
  const text = row.locator(".rv-cf").first();
  for (const cf of [text, row.locator(".rv-cf").nth(1)]) if ((await cf.getAttribute("data-done")) === "1") await cf.getByRole("button", { name: "Undo" }).click();
  // Under test: the line says so, and Use the agent's is drawn but disabled, saying why.
  const line = text.locator(".rv-rung");
  await expect(line).toContainText("testing…");
  const use = text.getByRole("button", { name: "Use the agent's" });
  await expect(use).toBeVisible();
  // Disabled, not hidden, until its tests pass; its title says why (the rung line).
  await expect(use).toBeDisabled();
  await expect(use).toHaveAttribute("title", /testing…/);
  const before = await line.boundingBox();
  const buttonsBefore = await text.locator(".rv-cfb").boundingBox();
  // Its tests pass: the same line, the same height, now passed; the resolution shows with its explanation.
  await page.request.post("/api/mock/review", { data: { ladder_tests: "passed", emit: { type: "conflict", diff: "D41" } } });
  await expect(line).toContainText("✓ its tests passed");
  expect((await line.boundingBox())?.height).toBe(before?.height);
  await expect(text.locator('.rv-cfs[data-side="offered"]')).toContainText("14 passed");
  await expect(use).toBeEnabled();
  // The binary file's rung kept the markers, and its Use is disabled, saying why.
  const binary = row.locator(".rv-cf").nth(1);
  await expect(binary.locator(".rv-rung")).toContainText("kept the markers");
  await expect(binary.getByRole("button", { name: "Use kiln's" })).toBeDisabled();
  expect((await text.locator(".rv-cfb").boundingBox())?.height).toBe(buttonsBefore?.height);
  // Offered: the gate row's Accept resolutions makes the new version, and the conflicts go.
  await page.request.post("/api/mock/review", { data: { ladder: "offered", emit: { type: "conflict", diff: "D41" } } });
  const accept = row.getByRole("button", { name: "Accept resolutions" });
  await expect(accept).toBeVisible();
  await accept.click();
  await expect(row.locator(".rv-cf")).toHaveCount(0);
  await expect(row.getByRole("button", { name: /^Approve/ })).toBeVisible();
});

test("Files says how fresh the change is, and Update stays in place, disabled with why, for a diff (W26-R16)", async ({ page }) => {
  const row = await open(page, "pipeline: dynamic matrices");
  const fresh = row.locator(".rv-bar[data-fresh]");
  await expect(fresh.locator(".rv-k[data-k='fresh']")).toHaveText(/^replays clean on main@[0-9a-f]{6}$/);
  // A replay runs no checks of its own: they're carried, and said to be.
  await expect(fresh.locator(".rv-k[data-k='checks']")).toHaveText("checks from v1");
  const update = fresh.getByRole("button", { name: /^Update/ });
  await expect(update).toBeVisible();
  await expect(update).toBeDisabled();
  await expect(update).toHaveAttribute("title", "a diff has no branch: its replay is its latest revision");
  // It sits on the gate row's line beneath, inside the section: nothing pokes out sideways.
  const [bar, sec] = await Promise.all([fresh.boundingBox(), row.locator(".rv-s").first().boundingBox()]);
  expect(bar && sec && bar.x >= sec.x - 1 && bar.x + bar.width <= sec.x + sec.width + 1).toBeTruthy();
});
