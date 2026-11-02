// Mission control's composer picks the agent as its page switches one, by a model of another
// harness: the permissions and effort on its line go along, named by that harness, as an agent's
// switch carries them (handoff.rs): the same latitude, never more, and the nearest effort.
import { expect, test, type Page } from "@playwright/test";

const line = (page: Page) => page.locator(".mc-how");
const value = (page: Page, k: "mode" | "model" | "effort") => line(page).locator(`.rs-v[data-kind="${k}"]`);

/** Opens a setting in place, unless a pick left it open. */
async function open(page: Page, k: "mode" | "model" | "effort") {
  if ((await value(page, k).getAttribute("aria-expanded")) !== "true") await value(page, k).click();
}

/** Moves a scale (Permissions, Effort) by its keys. */
async function slide(page: Page, k: "mode" | "effort", ...keys: string[]) {
  await open(page, k);
  const scale = line(page).getByRole("slider", { name: k === "mode" ? "Permissions" : "Effort" });
  for (const key of keys) await scale.press(key);
}

/** Picks the first model listed under another harness, `name` ("Codex", "Claude Code"): the agent starts there instead. */
async function model(page: Page, name: string) {
  await open(page, "model");
  await line(page)
    .getByRole("group", { name: `${name} models` })
    .getByRole("radio")
    .first()
    .click();
}

async function says(page: Page, mode: string, effort: string) {
  await expect(value(page, "mode")).toHaveAttribute("aria-label", `Permissions: ${mode}`);
  await expect(value(page, "effort")).toHaveAttribute("aria-label", `Effort: ${effort}`);
}

test.beforeEach(async ({ page }) => {
  await page.goto("/agents");
  await expect(value(page, "model")).toHaveAttribute("aria-label", /^Model: Opus/);
});

test("a harness switch carries the permissions and effort, and a switch back finds them as they were", async ({ page }) => {
  // Read only and the most effort, in Claude Code.
  await slide(page, "mode", "Home");
  await slide(page, "effort", "End");
  await says(page, "Plan", "Max");
  await model(page, "Codex");
  await expect(value(page, "model")).toHaveAttribute("aria-label", /^Model: GPT-6\.1 Sol/);
  await says(page, "Read only", "Max");
  await model(page, "Claude Code");
  await says(page, "Plan", "Max");
});

test("a round trip keeps what was chosen where the other harness has nothing as wide or as high", async ({ page }) => {
  // Claude Code's auto: Codex has nothing as wide, so its auto, the next one down.
  await slide(page, "mode", "Home", "ArrowRight", "ArrowRight", "ArrowRight", "ArrowRight");
  await says(page, "Auto", "choose");
  await model(page, "Codex");
  await says(page, "Auto", "choose");
  // Codex's ultra: Claude Code's max is the nearest.
  await slide(page, "effort", "End");
  await says(page, "Auto", "Ultra");
  await model(page, "Claude Code");
  await says(page, "Auto", "Max");
  await model(page, "Codex");
  await says(page, "Auto", "Ultra");
  // Chosen in Codex: its auto is Claude Code's accept edits.
  await slide(page, "mode", "Home", "ArrowRight", "ArrowRight");
  await says(page, "Auto", "Ultra");
  await model(page, "Claude Code");
  await says(page, "Accept edits", "Max");
});

test("skipping every check, confirmed for this start, is carried to the harness picked", async ({ page }) => {
  await open(page, "mode");
  await line(page).getByRole("slider", { name: "Permissions" }).press("End");
  await line(page).getByRole("button", { name: "Turn them off" }).click();
  await says(page, "Bypass", "choose");
  await model(page, "Codex");
  await says(page, "Full access", "choose");
  await model(page, "Claude Code");
  await says(page, "Bypass", "choose");
});

test("an agent that names no modes before it starts doesn't lose the permissions on the way through", async ({ page }) => {
  await slide(page, "mode", "Home");
  await slide(page, "effort", "End");
  await model(page, "Gemini CLI");
  await model(page, "Codex");
  await says(page, "Read only", "Max");
});

test("the permissions and effort are kept across visits, in the harness they were chosen in", async ({ page }) => {
  await model(page, "Codex");
  await slide(page, "mode", "Home");
  await slide(page, "effort", "End");
  await says(page, "Read only", "Ultra");
  await page.reload();
  await says(page, "Read only", "Ultra");
  await model(page, "Claude Code");
  await says(page, "Plan", "Max");
});

test("what the harness used last kept, before choices were carried, is where it starts until chosen again", async ({ page }) => {
  // Each harness kept its own before: the one used last's go along.
  await page.addInitScript(() => {
    const keep = (k: string, v: unknown) => localStorage.setItem(k, JSON.stringify(v));
    keep("kiln.agents.kind", "claude");
    keep("kiln.agents.modes", { claude: "plan", codex: "untrusted" });
    keep("kiln.agents.efforts", { claude: "max", codex: "low" });
  });
  await page.reload();
  await says(page, "Plan", "Max");
  await model(page, "Codex");
  await says(page, "Read only", "Max");
});
