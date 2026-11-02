/**
 * An experiment instead of an answer (docs/ci.md W20-R11), in the production build against the mock
 * (mock/experiments.ts: `ask-s2`'s question, CI's engine off). The setup opens as the experiment at
 * a glance, a line for each part saying what it's set to, as the agent suggested: started from there,
 * it takes one tap. Each part opens a page of its own (Done goes back): its metrics' thresholds, at
 * most or a % better than the baseline, the field and its switch there and disabled until one is
 * picked; a time limit in any unit; a check's command (no pipeline to pick, CI being off). A
 * problem shows on its part's line and holds Start. Nothing narrates; every line is one a finger
 * takes; nothing scrolls sideways or sits over another control on a phone or a desktop.
 */
import { expect, test, type Locator, type Page } from "@playwright/test";

/** Against a real kiln server (playwright.config.ts): these drive the mock's agents. */
const REAL = !!process.env.E2E_BASE_URL;

/** The shell's cards for what other agents wait on put away, as a person would: on a phone they sit over the conversation's end. */
async function putAwayCards(page: Page) {
  const away = page.getByRole("list", { name: "Waiting on you" }).getByRole("button", { name: /^Put away/ });
  await away
    .first()
    .waitFor({ timeout: 3000 })
    .catch(() => undefined);
  await expect(async () => {
    if ((await away.count()) > 0) await away.first().click({ timeout: 1000 });
    await expect(away).toHaveCount(0, { timeout: 500 });
  }).toPass({ timeout: 10_000 });
}

/** Nothing on the page, nor in the setup, wider than the page. */
async function nothingSideways(page: Page) {
  const doc = await page.evaluate(() => ({ scroll: document.documentElement.scrollWidth, client: document.documentElement.clientWidth }));
  expect(doc.scroll, "the page scrolls sideways").toBeLessThanOrEqual(doc.client);
  const over = await page
    .locator(".ch-x *")
    .evaluateAll((els) => els.filter((e) => e.getBoundingClientRect().right > document.documentElement.clientWidth + 0.5).map((e) => `${e.tagName.toLowerCase()}.${e.className}`));
  expect(over, "something in the setup is wider than the page").toEqual([]);
}

/** `a` and `b` share no pixel: neither covers the other. */
async function apart(a: Locator, b: Locator) {
  const [x, y] = [await a.boundingBox(), await b.boundingBox()];
  expect(x && y, "both shown").toBeTruthy();
  if (!x || !y) return;
  const overlap = x.x < y.x + y.width - 0.5 && y.x < x.x + x.width - 0.5 && x.y < y.y + y.height - 0.5 && y.y < x.y + x.height - 0.5;
  expect(overlap, `${JSON.stringify(x)} overlaps ${JSON.stringify(y)}`).toBe(false);
}

/** Every line of what's shown is one a finger takes. */
async function tallLines(page: Page, least: number) {
  const heights = await page.locator(".ch-x :is(.ch-sum-line, .ch-item, .ch-field, .ch-toggle)").evaluateAll((els) => els.map((e) => Math.round(e.getBoundingClientRect().height)));
  expect(heights.length).toBeGreaterThan(0);
  expect(Math.min(...heights), `the lines' heights: ${heights.join(", ")}`).toBeGreaterThanOrEqual(least);
}

/** A still of the setup as it shows, in a window tall enough for it, clear of the composer. */
async function still(page: Page, path: string) {
  const size = page.viewportSize();
  if (!size) return;
  await page.setViewportSize({ width: size.width, height: 3000 });
  // Laid out again at that height before it's measured.
  await page.waitForTimeout(300);
  await page.locator(".ch-x").evaluate((el) => el.scrollIntoView({ block: "center" }));
  await page.waitForTimeout(150);
  const [top, foot] = [await page.locator(".ch-x").boundingBox(), await page.locator(".ch-foot").boundingBox()];
  if (top && foot) await page.screenshot({ path, clip: { x: 0, y: Math.max(0, top.y - 16), width: size.width, height: foot.y + foot.height - Math.max(0, top.y - 16) + 16 } });
  await page.setViewportSize(size);
}

const started = async (page: Page) => (await (await page.request.get("/api/mock/experiments")).json()) as Record<string, unknown>[];
const line = (page: Page, name: string) => page.getByRole("list", { name: "The experiment" }).getByRole("button", { name: new RegExp(`^${name}:`) });

async function openSetup(page: Page) {
  await page.goto("/agents/ask-lead?task=ask-s2");
  await putAwayCards(page);
  await page.getByRole("button", { name: "Experiment…" }).click();
  await expect(page.getByRole("list", { name: "The experiment" })).toBeVisible();
}

test.beforeEach(async ({ page }) => {
  test.skip(REAL, "the mock's agents");
  // Each test from the same question, nothing started yet.
  await page.request.post("/api/mock/asks");
  await page.request.post("/api/mock/experiments");
});

test("w20_r11 the experiment at a glance, as the agent suggested it: started in one tap", async ({ page }, info) => {
  await openSetup(page);
  await expect(line(page, "Variants")).toHaveAccessibleName("Variants: Each alone + baseline");
  await expect(line(page, "Checks")).toHaveAccessibleName("Checks: ci/gate.sh --quick");
  await expect(line(page, "Metrics")).toHaveAccessibleName("Metrics: wall_s");
  await expect(line(page, "Runs")).toHaveAccessibleName("Runs: 3 per variant · 4 at once");
  await expect(line(page, "When done")).toHaveAccessibleName("When done: Bring the results back");
  // It says what it's set to, and nothing about itself.
  for (const narration of ["KILN_METRICS", "suggested", "worktree", "exits 0"]) await expect(page.locator(".ch-x")).not.toContainText(narration);
  await tallLines(page, 44);
  await nothingSideways(page);
  // Its actions end where its lines do (on a wide screen too).
  const [lines, start] = [await page.locator(".ch-x").boundingBox(), await page.getByRole("button", { name: "Start experiment" }).boundingBox()];
  expect(lines && start && start.x + start.width <= lines.x + lines.width + 1, "Start ends where the lines do").toBe(true);
  await still(page, info.outputPath(`experiment-summary-${info.project.name}.png`));

  await page.getByRole("button", { name: "Start experiment" }).click();
  await expect.poll(async () => (await started(page)).length).toBe(1);
  const [sent] = await started(page);
  expect(sent?.agent).toBe("ask-s2");
  expect(sent?.budget_seconds).toBeUndefined();
  expect(sent?.criteria).toEqual([
    { name: "command-1", kind: "command", command: "ci/gate.sh --quick", required: true },
    { name: "wall_s", kind: "metric", metric: "wall_s", better: "lower", required: false, threshold: { better: 0.1 } },
  ]);
});

test("w20_r11 each part on a page of its own: a check, thresholds, a time limit in any unit", async ({ page }, info) => {
  await openSetup(page);
  const done = page.getByRole("button", { name: "Done" });

  // Variants: the options in play, how they're combined, the baseline.
  await line(page, "Variants").click();
  await expect(page.locator(".ch-x h3")).toHaveText("Variants");
  await expect(page.getByRole("group", { name: "Options in play" }).getByRole("checkbox")).toHaveCount(2);
  await expect(page.getByRole("combobox", { name: "Combine" })).toHaveValue("each");
  await expect(page.getByRole("switch", { name: "Baseline" })).toHaveAttribute("aria-checked", "true");
  await tallLines(page, 40);
  await nothingSideways(page);
  await still(page, info.outputPath(`experiment-variants-${info.project.name}.png`));
  await done.click();

  // Checks: the agent's command, a must-pass switch, no pipeline to pick with CI off.
  await line(page, "Checks").click();
  await expect(page.locator(".ch-x h3")).toHaveText("Checks");
  await expect(page.getByRole("textbox", { name: "Check 1: command" })).toHaveValue("ci/gate.sh --quick");
  await expect(page.getByRole("switch", { name: "Check 1: must pass" })).toHaveAttribute("aria-checked", "true");
  await expect(page.getByRole("combobox", { name: "Check 1: kind" }), "no pipelines while CI's engine is off").toHaveCount(0);
  await expect(page.locator(".ch-foot").getByRole("button", { name: "Back" }), "a part's page has Done alone").toHaveCount(0);
  await done.click();

  // Metrics: one of one's own, its threshold's field and its switch disabled until one is picked.
  await line(page, "Metrics").click();
  await expect(page.locator(".ch-x h3")).toHaveText("Metrics");
  await expect(page.getByRole("combobox", { name: "Metric 1: threshold" })).toHaveValue("relative");
  await expect(page.getByRole("spinbutton", { name: "Metric 1: threshold value" })).toHaveValue("10");
  await page.getByRole("button", { name: "Add a metric" }).click();
  await page.getByRole("textbox", { name: "Metric 2: name" }).fill("rss_mb");
  const value = page.getByRole("spinbutton", { name: "Metric 2: threshold value" });
  const must = page.getByRole("switch", { name: "Metric 2: must meet the threshold" });
  await expect(value).toBeDisabled();
  await expect(must).toBeDisabled();
  await page.getByRole("combobox", { name: "Metric 2: threshold" }).selectOption({ label: "At most" });
  await expect(value).toBeEnabled();
  await expect(must).toBeEnabled();
  await value.fill("512");
  await must.click();
  await expect(must).toHaveAttribute("aria-checked", "true");
  for (const n of [1, 2]) {
    const [better, kind, amount, meet] = [
      page.getByRole("combobox", { name: `Metric ${n}: better` }),
      page.getByRole("combobox", { name: `Metric ${n}: threshold` }),
      page.getByRole("spinbutton", { name: `Metric ${n}: threshold value` }),
      page.getByRole("switch", { name: `Metric ${n}: must meet the threshold` }),
    ];
    for (const [a, b] of [
      [better, kind],
      [kind, amount],
      [amount, meet],
    ] as const)
      await apart(a, b);
  }
  await tallLines(page, 36);
  await nothingSideways(page);
  if (info.project.name.startsWith("phone")) {
    // A narrow phone's 360px: a control too wide beside its words goes under them, at the line's end.
    const size = page.viewportSize();
    await page.setViewportSize({ width: 360, height: size?.height ?? 800 });
    await nothingSideways(page);
    await apart(page.getByRole("combobox", { name: "Metric 1: threshold" }), page.locator(".ch-x").getByText("Threshold").first());
    await apart(page.getByRole("spinbutton", { name: "Metric 1: threshold value" }), page.getByRole("switch", { name: "Metric 1: must meet the threshold" }));
    if (size) await page.setViewportSize(size);
  }
  await still(page, info.outputPath(`experiment-metrics-${info.project.name}.png`));
  await done.click();
  await expect(line(page, "Metrics")).toHaveAccessibleName("Metrics: wall_s, rss_mb");

  // Runs: a time limit of two hours.
  await line(page, "Runs").click();
  await expect(page.locator(".ch-x h3")).toHaveText("Runs");
  await page.getByRole("spinbutton", { name: "Time limit" }).fill("2");
  await page.getByRole("combobox", { name: "Time limit in" }).selectOption("hours");
  await expect(page.getByRole("combobox", { name: "Time limit in" }).locator("option")).toHaveText(["seconds", "minutes", "hours", "days", "weeks", "months", "years"]);
  await expect(page.locator(".ch-x")).toContainText("adacachy");
  await tallLines(page, 40);
  await nothingSideways(page);
  await still(page, info.outputPath(`experiment-runs-${info.project.name}.png`));
  await done.click();
  await expect(line(page, "Runs")).toHaveAccessibleName("Runs: 3 per variant · 4 at once · 2 hours");

  await page.getByRole("button", { name: "Start experiment" }).click();
  await expect.poll(async () => (await started(page)).length).toBe(1);
  const [sent] = await started(page);
  expect(sent?.budget_seconds).toBe(7_200);
  expect(sent?.criteria).toEqual([
    { name: "command-1", kind: "command", command: "ci/gate.sh --quick", required: true },
    { name: "wall_s", kind: "metric", metric: "wall_s", better: "lower", required: false, threshold: { better: 0.1 } },
    { name: "rss_mb", kind: "metric", metric: "rss_mb", better: "lower", required: true, threshold: { at_most: 512 } },
  ]);
});

test("w20_r11 a problem shows on its part's line, and holds Start", async ({ page }) => {
  await openSetup(page);
  await line(page, "Checks").click();
  await page.getByRole("textbox", { name: "Check 1: command" }).fill("");
  await page.getByRole("button", { name: "Done" }).click();
  await expect(line(page, "Checks")).toHaveAttribute("data-problem", "true");
  await expect(line(page, "Checks")).toHaveAccessibleName("Checks: None");
  await expect(page.locator(".ch-problem")).toHaveText("Give the command to run.");
  await expect(page.getByRole("button", { name: "Start experiment" })).toBeDisabled();
  await nothingSideways(page);
});
