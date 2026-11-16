// An agent definition's experiments changed through the server's operations (`agentdefs.experiments.*`, kiln plan
// 01a1cd8e862e7169a074db3d5fa33f3d P7.1), against the mock, which answers them as crates/kiln/src/agentdefs/experiments
// does (mock/agentdefs.ts), in the Engine tab's agents' cards: each change shown as the server answered it, and the list
// read again says the same; a second live one refused in the server's words, and resuming beside one; a host variant
// set up with no Agentfile or size; a promotion that keeps its split's weights and makes the definition its next
// version; a delete asked first, gone for good, with no Undo.
import { expect as expectNow, test, type Locator, type Page } from "@playwright/test";

const expect = expectNow.configure({ timeout: 15_000 });

/** Against a real kiln server (playwright.config.ts): these drive the mock's definitions. */
const REAL = !!process.env.E2E_BASE_URL;
const G = 2 ** 30;

type Listed = {
  id: string;
  name: string;
  state: string;
  version: number;
  promoted?: string;
  criteria: { id: string; name: string }[];
  variants: { id: string; engine: string; agentfile: string | null; cpus?: number; memory?: number; weight: number; current: boolean; measures?: unknown }[];
};

/** Console errors as the page prints them, but the browser's own line for each refusal these tests ask for. */
function errors(page: Page) {
  const said: string[] = [];
  page.on("console", (m) => {
    if (m.type() === "error" && !/the server responded with a status of 4(09|22)/.test(m.text())) said.push(m.text());
  });
  return said;
}

async function defId(page: Page, name: string): Promise<string> {
  const list = (await (await page.request.get("/api/agentdefs")).json()) as { agents: { id: string; name: string }[] };
  return list.agents.find((a) => a.name === name)?.id ?? "";
}
/** A definition's experiments as the server lists them, newest first. */
async function listed(page: Page, def: string): Promise<Listed[]> {
  return ((await (await page.request.get(`/api/agentdef-experiments?id=${def}`)).json()) as Record<string, Listed[]>)[def] ?? [];
}

/** rev-big's Engine tab (a session of reviewer's, in a microVM): every agent a card, its settings and experiments shown whole. */
async function engineTab(page: Page): Promise<Locator> {
  await page.goto("/agents/rev-big");
  await expect(page.getByRole("textbox", { name: "Message to the agent", exact: true })).toBeEnabled({ timeout: 30_000 });
  const tab = page
    .getByRole("tab", { name: /^Engine/ })
    .filter({ visible: true })
    .first();
  await tab.scrollIntoViewIfNeeded();
  await tab.click();
  await expect(tab).toHaveAttribute("aria-selected", "true");
  return page.locator(".hx-inspector-body").filter({ visible: true }).first();
}
const card = (body: Locator, def: string) => body.locator(`[aria-label="${def}: settings and experiments"]`);
/** An experiment's line, and what it holds under it (its split, variants and criteria). */
const line = (card: Locator, name: string) => card.locator("li.df-exp", { hasText: name });
const under = (line: Locator) => line.locator("xpath=following-sibling::li[1]");
/** A toast by its title (one that says a failure is an alert, not a list item). */
const toast = (page: Page, title: string) => page.getByRole("list", { name: "Notifications" }).locator("li").filter({ hasText: title });

test.beforeEach(async ({ page }) => {
  test.skip(REAL, "the mock's definitions");
  // Each from the seeded experiments, and the tests after these too.
  await page.request.post("/api/mock/agentdef-experiments");
});
test.afterEach(async ({ page }) => {
  if (!REAL) await page.request.post("/api/mock/agentdef-experiments");
});

test("p7_1 a second live experiment is refused in the server's words, and so is resuming one beside it", async ({ page }) => {
  const said = errors(page);
  const reviewer = await defId(page, "reviewer");
  const body = await engineTab(page);
  const mine = card(body, "reviewer");
  await mine.getByRole("button", { name: "New experiment on reviewer" }).click();
  const setup = mine.locator("li.df-exp-new");
  await setup.getByRole("button", { name: "Start" }).click();
  // The server's words where it was asked, and nothing started.
  await expect(setup.getByRole("alert")).toHaveText("Right-size its microVM is live on this agent, and one at a time splits its sessions: pause, stop or promote it first");
  expect((await listed(page, reviewer)).map((e) => e.id)).toEqual(["reviewer-size", "reviewer-image"]);
  await setup.getByRole("button", { name: "Cancel" }).click();
  await expect(setup).toHaveCount(0);
  // Resumed beside the live one: refused, saying why, and paused as it was.
  const image = line(mine, "rust-dev v9's image");
  await image.getByRole("button", { name: "Resume rust-dev v9's image" }).click();
  await expect(toast(page, "Couldn't resume rust-dev v9's image")).toContainText("Right-size its microVM is live on this agent");
  await expect(image.locator(".ms-word")).toHaveText("Paused");
  expect((await listed(page, reviewer)).find((e) => e.id === "reviewer-image")?.state).toBe("paused");
  expect(said, said.join("\n")).toEqual([]);
});

test("p7_1 a host variant's setup has no Agentfile or size, and it starts as the server took it", async ({ page }) => {
  const said = errors(page);
  const terse = await defId(page, "about-terse");
  const body = await engineTab(page);
  const theirs = card(body, "about-terse");
  await expect(theirs.locator("li.df-head", { hasText: "Experiments" })).toContainText("none");
  await theirs.getByRole("button", { name: "New experiment on about-terse" }).click();
  const setup = theirs.locator("li.df-exp-new");
  const variants = setup.locator("li.df-vline");
  await expect(variants).toHaveCount(2);
  const second = variants.nth(1);
  // A microVM's Agentfile (kiln's for its harness, as it names none) and size; the host has neither.
  await expect(second.getByRole("combobox", { name: "Agentfile" })).toHaveValue("harness-claude");
  await second.getByRole("group", { name: "Engine" }).getByRole("button", { name: "host" }).click();
  for (const field of ["Agentfile", "vCPUs", "Memory"]) await expect(second.getByRole("combobox", { name: field })).toHaveCount(0);
  await expect(variants.locator(".df-vname")).toHaveText(["microVM", "host"]);
  await setup.getByRole("textbox", { name: "Its name" }).fill("Where it runs");
  const sent = page.waitForRequest((r) => r.method() === "POST" && r.url().endsWith(`/api/agentdefs/${terse}/experiments`));
  await setup.getByRole("button", { name: "Start" }).click();
  expect((await sent).postDataJSON()).toEqual({
    name: "Where it runs",
    variants: [
      { weight: 80, agentfile: "harness-claude", cpus: 2, memory: 4 * G },
      { weight: 20, engine: "host" },
    ],
  });
  await expect(setup).toHaveCount(0);
  const made = line(theirs, "Where it runs");
  await expect(made.locator(".ms-word")).toHaveText("Live");
  await expect(made).toContainText("split v1");
  await expect(toast(page, "Started Where it runs")).toBeVisible();
  // As the server keeps it: the host's variant without an Agentfile or size; no measures yet, and none shown.
  const [kept] = await listed(page, terse);
  expect(kept?.variants.map((v) => [v.engine, v.agentfile, v.cpus, v.memory, v.weight, v.current])).toEqual([
    ["microvm", "harness-claude", 2, 4 * G, 80, true],
    ["host", null, undefined, undefined, 20, false],
  ]);
  await expect(under(made).locator(".df-vname")).toHaveText(["microVM", "host"]);
  await expect(under(made).locator(".ms-measure")).toHaveCount(0);
  expect(said, said.join("\n")).toEqual([]);
});

test("p7_1 a promotion keeps its split's weights, makes its variant current and its definition the next version", async ({ page }) => {
  const said = errors(page);
  const triage = await defId(page, "pr-triage");
  const before = (await (await page.request.get(`/api/agentdefs/${triage}`)).json()) as { version: number };
  const body = await engineTab(page);
  const theirs = card(body, "pr-triage");
  const six = line(theirs, "Six sizes");
  // Its Agentfiles by their names (the server names them by id).
  const big = under(six).locator("li.df-vline", { hasText: "rust-dev · 4 vCPU · 8 GiB" });
  await big.getByRole("button", { name: "Promote", exact: true }).click();
  await big.getByRole("button", { name: "Promote rust-dev · 4 vCPU · 8 GiB" }).click();
  await expect(six.locator(".ms-word")).toHaveText("Promoted");
  await expect(big.locator(".df-vline-top")).toContainText("current");
  await expect(under(six).locator(".df-vline-weight .df-head-v")).toHaveText([/^20% /, /^20% /, /^20% /, /^15% /, /^15% /, /^10% /]);
  await expect(toast(page, "Promoted rust-dev · 4 vCPU · 8 GiB")).toBeVisible();
  const now = (await listed(page, triage)).find((e) => e.id === "triage-sizes");
  expect(now?.state).toBe("promoted");
  expect(now?.promoted).toBe("19d6");
  expect(now?.variants.map((v) => v.weight)).toEqual([20, 20, 20, 15, 15, 10]);
  expect(now?.variants.filter((v) => v.current).map((v) => v.id)).toEqual(["19d6"]);
  // Its deployment the definition's next version, which its card says.
  const after = (await (await page.request.get(`/api/agentdefs/${triage}`)).json()) as { version: number; cpus?: number; memory?: number };
  expect([after.version, after.cpus, after.memory]).toEqual([before.version + 1, 4, 8 * G]);
  await expect(theirs.locator("li.df-head", { hasText: "Settings" })).toContainText(`v${after.version}`);
  // As it was, for the tests after it.
  expect((await page.request.post(`/api/agentdefs/${triage}/restore`, { data: { version: before.version } })).ok()).toBe(true);
  expect(said, said.join("\n")).toEqual([]);
});

test("p7_1 a delete is asked first, then gone for good, with no Undo", async ({ page }) => {
  const said = errors(page);
  const reviewer = await defId(page, "reviewer");
  const body = await engineTab(page);
  const mine = card(body, "reviewer");
  const image = line(mine, "rust-dev v9's image");
  await image.getByRole("button", { name: "Delete rust-dev v9's image" }).click();
  await expect(image.getByRole("group", { name: "Delete rust-dev v9's image?" })).toBeVisible();
  await image.getByRole("button", { name: "Delete rust-dev v9's image" }).click();
  await expect(line(mine, "rust-dev v9's image")).toHaveCount(0);
  const done = toast(page, "Deleted rust-dev v9's image");
  await expect(done).toBeVisible();
  await expect(done.getByRole("button", { name: "Undo" })).toHaveCount(0);
  expect((await listed(page, reviewer)).map((e) => e.id)).toEqual(["reviewer-size"]);
  // Read again: still gone.
  const again = card(await engineTab(page), "reviewer");
  await expect(line(again, "Right-size its microVM")).toHaveCount(1);
  await expect(line(again, "rust-dev v9's image")).toHaveCount(0);
  expect(said, said.join("\n")).toEqual([]);
});

test("p7_1 each change is the server's: its name, its split's next version, a variant and a criterion each way, paused and stopped", async ({ page }) => {
  const said = errors(page);
  const reviewer = await defId(page, "reviewer");
  const kept = async () => (await listed(page, reviewer)).find((e) => e.id === "reviewer-size");
  const body = await engineTab(page);
  const mine = card(body, "reviewer");
  // Newest first: the live one.
  const size = mine.locator("li.df-exp").first();
  await expect(size).toContainText("Right-size its microVM");
  // Renamed.
  await size.getByRole("button", { name: "Rename Right-size its microVM" }).click();
  await size.getByRole("textbox", { name: "Its name" }).fill("Two sizes");
  await size.getByRole("textbox", { name: "Its name" }).press("Enter");
  await expect(size.locator(".df-vm-name")).toHaveText("Two sizes");
  await expect.poll(async () => (await kept())?.name).toBe("Two sizes");
  // A split moved and applied: its next version, as the server answered it.
  const held = under(size);
  const small = held.getByRole("slider", { name: "Small's weight" });
  await small.focus();
  for (let i = 0; i < 10; i++) await small.press("ArrowRight");
  await held.getByRole("button", { name: "Apply the split" }).click();
  await expect(held.getByRole("button", { name: "Apply the split" })).toHaveCount(0);
  await expect(size).toContainText("split v2");
  await expect(toast(page, "Two sizes: split v2, 60/40")).toBeVisible();
  expect((await kept())?.variants.map((v) => v.weight)).toEqual([60, 40]);
  // A variant added (the last one's deployment, at 0) and taken out again.
  await held.getByRole("button", { name: "Add a variant" }).click();
  await expect(held.locator("li.df-vline")).toHaveCount(3);
  await expect.poll(async () => (await kept())?.variants.map((v) => v.weight)).toEqual([60, 40, 0]);
  await held.getByRole("button", { name: "Remove 4 vCPU · 8 GiB" }).click();
  await expect(held.locator("li.df-vline")).toHaveCount(2);
  await expect.poll(async () => (await kept())?.variants.length).toBe(2);
  expect((await kept())?.version).toBe(2);
  // A criterion added, as kiln's monitors read it, and taken out again.
  await held.getByRole("button", { name: "Add a criterion" }).click();
  await held.getByRole("textbox", { name: "Name" }).fill("Few failures");
  const expr = held.getByRole("textbox", { name: "Expression" });
  await expr.click();
  await page.keyboard.press("ControlOrMeta+End");
  await page.keyboard.type("< 10");
  await held.getByRole("button", { name: "Add", exact: true }).click();
  await expect(held.locator(".df-crit-name")).toHaveText(["Warm start", "Failed sessions", "Peak memory", "Few failures"]);
  await expect.poll(async () => (await kept())?.criteria.map((c) => c.name)).toEqual(["Warm start", "Failed sessions", "Peak memory", "Few failures"]);
  await held.getByRole("button", { name: "Remove Few failures" }).click();
  await expect(held.locator(".df-crit-name")).toHaveText(["Warm start", "Failed sessions", "Peak memory"]);
  await expect.poll(async () => (await kept())?.criteria.length).toBe(3);
  // Paused, resumed, then stopped (asked first): its split no longer changes.
  await size.getByRole("button", { name: "Pause Two sizes" }).click();
  await expect(size.locator(".ms-word")).toHaveText("Paused");
  await expect.poll(async () => (await kept())?.state).toBe("paused");
  await size.getByRole("button", { name: "Resume Two sizes" }).click();
  await expect(size.locator(".ms-word")).toHaveText("Live");
  await expect.poll(async () => (await kept())?.state).toBe("live");
  await size.getByRole("button", { name: "Stop Two sizes" }).click();
  await size.getByRole("group", { name: "Stop Two sizes?" }).getByRole("button", { name: "Stop Two sizes" }).click();
  await expect(size.locator(".ms-word")).toHaveText("Stopped");
  await expect(held.getByRole("slider", { name: "Small's weight" })).toBeDisabled();
  await expect(held.getByRole("button", { name: "Add a variant" })).toHaveCount(0);
  await expect.poll(async () => (await kept())?.state).toBe("stopped");
  expect(said, said.join("\n")).toEqual([]);
});
