// An agent definition's values edited in place (kiln's principle 5; kiln plan 01a1cd8e862e7169a074db3d5fa33f3d,
// P5.2), against the mock: a save carries the version it was based on, so one made by someone else in between
// shows as a diff on its row, theirs and yours, and is never overwritten unasked; then yours, saved on theirs.
import { expect as expectNow, test, type Page } from "@playwright/test";

const expect = expectNow.configure({ timeout: 15_000 });

function errors(page: Page) {
  const said: string[] = [];
  page.on("console", (m) => {
    if (m.type() === "error" && !m.text().includes("409")) said.push(m.text());
  });
  return said;
}

test("a save made in between is shown as a diff, then yours goes on the current version", async ({ page }) => {
  const said = errors(page);
  const list = (await (await page.request.get("/api/agentdefs")).json()) as { agents: { id: string; name: string; version: number; model: string | null }[] };
  const reviewer = list.agents.find((a) => a.name === "reviewer")!;
  // A model other than the one it has, so the save is a change whatever ran before.
  const mine = reviewer.model === "claude-sonnet-5-5" ? "claude-opus-5-5" : "claude-sonnet-5-5";
  await page.goto("/agents/rev-big?tab=engine");
  const body = page.locator(".hx-inspector-body").filter({ visible: true }).first();
  const tab = page
    .getByRole("tab", { name: /^Engine/ })
    .filter({ visible: true })
    .first();
  await tab.scrollIntoViewIfNeeded();
  await tab.click();
  const model = body.getByRole("button", { name: /^model / }).first();
  await expect(model).toBeVisible();
  // Someone else saves first.
  const theirs = await page.request.patch(`/api/agentdefs/${reviewer.id}`, { data: { instructions: `Theirs at ${Date.now()}` } });
  expect(theirs.ok()).toBe(true);
  const now = ((await theirs.json()) as { version: number }).version;
  // Yours, based on what the page read.
  await model.click();
  const field = body.getByRole("textbox", { name: "model" }).first();
  await field.fill(mine);
  await field.press("Enter");
  const clash = body.getByRole("group", { name: `Saved as v${now} since you read it` });
  await expect(clash).toBeVisible();
  await expect(clash).toContainText("instructions");
  await clash.getByRole("button", { name: `Save mine on v${now}` }).click();
  await expect(clash).toHaveCount(0);
  const after = (await (await page.request.get(`/api/agentdefs/${reviewer.id}`)).json()) as { version: number; model: string; instructions: string };
  expect(after.version).toBe(now + 1);
  expect(after.model).toBe(mine);
  expect(after.instructions).toMatch(/^Theirs at /);
  expect(said, said.join("\n")).toEqual([]);
});
