/**
 * Ship's Pipelines (docs/ci.md W29-R01 to R08, as Ada approved its stills on 2026-10-31), against
 * the mock's three repositories, at the stills' widths: 1280 on a desktop project, 390 on a phone's,
 * in Chromium and (E2E_WEBKIT=1) WebKit. Pipelines is Ship's third tab; its rows are Ship's by
 * repository, needs-someone first, DRAFT beside a draft's title, and a New row last; `?repo=&path=`
 * opens a row and brings it into view; nothing scrolls sideways.
 */
import { expect, test, type Page } from "@playwright/test";

const REAL = !!process.env.E2E_BASE_URL;
test.skip(REAL, "drives the mock's pipelines");

/** The stills' widths: 1280 for a desktop, 390 for a phone. */
const atStillWidth = (page: Page, isMobile: boolean) => page.setViewportSize(isMobile ? { width: 390, height: 844 } : { width: 1280, height: 900 });

/** The app's own requests waiting on you come and go over the page's foot: set aside, as Deploys' spec does. */
const notices = (page: Page) => page.addStyleTag({ content: ".rq-viewport { display: none !important; }" });

const list = (page: Page) => page.getByRole("list", { name: "Workflows and actions by repository" });
/** A repository's rows, in the list's order, up to the next heading. */
const rowsOf = (page: Page, repo: string) => list(page).locator(`.ppl-row[data-key^="${repo}:"]:not(.ppl-new)`);

/** How far the page scrolls sideways. */
const sideways = (page: Page) => page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);

test("Pipelines is Ship's third tab: repositories needing someone first, each row Ship's, DRAFT beside a draft's title, New last", async ({ page, isMobile }) => {
  await atStillWidth(page, isMobile);
  await page.goto("/ship/pipelines");
  await notices(page);
  const views = page.getByRole("navigation", { name: "Ship views" });
  await expect(views.getByRole("link")).toHaveText(["Commits", "Deploys", "Pipelines"]);
  await expect(views.getByRole("link", { name: "Pipelines" })).toHaveClass(/is-current/);
  await expect(page.getByPlaceholder("Repository, workflow, action, kind, machine…")).toBeVisible();

  // Repositories with rows needing someone first, then by name; each heading says what's there.
  await expect(list(page).locator(".dp-proj > span:first-child")).toHaveText(["hyper-light/ci-kit", "hyper-light/kiln", "hyper-light/quiver"]);
  await expect(list(page).locator(".dp-proj > span:last-child")).toHaveText([
    "shared · 6 actions · 1 workflow · 1 draft",
    "7 workflows · 1 failing · 1 waits on you",
    "3 workflows · ci-kit v2.4.0 is out",
  ]);
  await expect(list(page).locator(".dp-proj > span[data-needs]")).toHaveCount(2);

  // kiln's: Review, then Failing, then the rest by name.
  const kiln = rowsOf(page, "hyper-light/kiln");
  await expect(kiln.locator(".ppl-title")).toHaveText(["stress", "e2e", "ci", "GitHub CI", "nightly", "release", "smoke"]);
  await expect(kiln.locator(".ship-stand")).toHaveText(["Review", "Failing", "Passing", "Not run here", "Passing", "Passing", "Passing"]);
  // DRAFT sits beside a draft's title, and nowhere else.
  await expect(kiln.locator(".ppl-draft")).toHaveCount(1);
  await expect(kiln.first().locator(".ppl-name")).toContainText("stressDRAFT");
  // Where each is: a draft's change, else its branch; an action's newest tag.
  await expect(kiln.first().locator(".ppl-ref[data-ref=change]")).toHaveText("#214");
  await expect(kiln.nth(2).locator(".ppl-ref")).toHaveText("main");
  await expect(rowsOf(page, "hyper-light/ci-kit").filter({ hasText: "setup-rust" }).locator(".ppl-ref[data-ref=tag]")).toHaveText("v2.4.0");
  // Three pips each: Checks, Runs on, Latest.
  await expect(kiln.nth(1).locator(".js-p")).toHaveCount(3);

  // Each repository ends with its New row.
  for (const repo of ["hyper-light/ci-kit", "hyper-light/kiln", "hyper-light/quiver"]) {
    const keys = await list(page)
      .locator(".ppl-row")
      .evaluateAll((els) => els.map((e) => e.getAttribute("data-key") ?? ""));
    const mine = keys.filter((k) => k.startsWith(`${repo}:`));
    expect(mine.at(-1)).toBe(`${repo}:new`);
  }
  expect(await sideways(page)).toBe(0);
});

test("?repo=&path= opens that row and brings it into view", async ({ page, isMobile }) => {
  await atStillWidth(page, isMobile);
  await page.goto("/ship/pipelines?repo=hyper-light%2Fkiln&path=.kiln%2Fci.yaml");
  await notices(page);
  const row = list(page).locator('.ppl-row[data-key="hyper-light/kiln:.kiln/ci.yaml"]');
  await expect(row.locator(".ship-main")).toHaveAttribute("aria-expanded", "true");
  await expect(row.locator(".ppl-kicker")).toContainText(".kiln/ci.yaml · kiln's dialect · main · a3f2c1d");
  await expect(row).toBeInViewport();
  // Closing it takes it out of the address; another opens in its place.
  await row.locator(".ship-go").click();
  await expect(page).not.toHaveURL(/path=/);
  expect(await sideways(page)).toBe(0);
});

/** The opened row's pane, at `path` of `repo` (a draft's with `draft`). */
async function openPane(page: Page, isMobile: boolean, repo: string, path: string, draft?: string) {
  await atStillWidth(page, isMobile);
  await page.goto(`/ship/pipelines?repo=${encodeURIComponent(repo)}&path=${encodeURIComponent(path)}${draft ? `&draft=${draft}` : ""}`);
  await notices(page);
  const pane = page.getByRole("region", { name: "Pipeline" });
  await expect(pane.locator(".jp-strip")).toBeVisible({ timeout: 15_000 });
  return pane;
}

test("an opened workflow is the jobs pane's anatomy: the strip, its groups, the picked line's detail beside them or under its line", async ({ page, isMobile }) => {
  const pane = await openPane(page, isMobile, "hyper-light/kiln", ".kiln/ci.yaml");
  await expect(pane.locator(".jp-g > span:first-child")).toHaveText(["Triggers", "Stage 1", "Stage 2", "Uses", "Secrets and variables", "Runs", "Settings"]);
  await expect(pane.locator('.jp-g[data-group="Stage 1"] > span:last-child')).toHaveText("3 jobs at once");
  await expect(pane.locator('.jp-g[data-group="Stage 2"] > span:last-child')).toHaveText("after rust and ui");
  // The first job is picked: YAML · Plan · Last run, its lines tinted in the code.
  await expect(pane.locator(".jp-j[data-at]")).toHaveAttribute("data-key", "jobs.rust");
  const detail = pane.locator(".ppl-detail");
  await expect(detail.getByRole("tab")).toHaveText(["YAML", "Plan", "Last run"]);
  await expect(detail.locator(".ppl-code li[data-at]").first()).toContainText("rust:");
  await expect(detail.locator(".ppl-code li[data-at]")).toHaveCount(11);
  // Beside the list on a desktop; right under its line on a phone.
  if (isMobile) await expect(pane.locator('.jp-j[data-key="jobs.rust"] + li.jp-detail .ppl-detail')).toBeVisible();
  else await expect(pane.locator(".ppl-side .ppl-detail")).toBeVisible();
  // A trigger: its Filters, YAML and Plan; the plan is its event's.
  await pane.locator('.jp-j[data-key="on.push"] .jp-line').click();
  await expect(detail.getByRole("tab")).toHaveText(["Filters", "YAML", "Plan"]);
  await expect(detail.locator(".ppl-lines")).toContainText("branches main");
  const asked = page.waitForRequest((r) => r.url().endsWith("/api/ci/plan-preview") && r.method() === "POST");
  await detail.getByRole("tab", { name: "Plan" }).click();
  expect((await asked).postDataJSON()).toMatchObject({ repo: "hyper-light/kiln", path: ".kiln/ci.yaml", event: "push" });
  await expect(detail.locator(".ppl-lines li")).toHaveCount(5);
  await expect(detail.locator(".ppl-lines li").filter({ hasText: "release-binary" })).toContainText("3 legs");
  // The verbs: Run, and the rest dimmed, never hidden; a dimmed one says why.
  const verbs = pane.locator(".ppl-verbs button");
  await expect(verbs).toHaveText(["Run", "Save draft", "Ready", "Discard"]);
  await expect(pane.locator(".ppl-verbs button[data-dim]")).toHaveCount(3);
  // Dimmed is aria-disabled, which Playwright won't click; a person's click still says why.
  await verbs.filter({ hasText: "Ready" }).click({ force: true });
  await expect(pane.locator(".jp-said")).toHaveText("Ready is for a draft (W29-R12)");
  await expect(page.locator("[role=dialog], .cm-tooltip")).toHaveCount(0);
  expect(await sideways(page)).toBe(0);
});

test("an action's pane: its inputs, steps, versions, who uses it and its access; Changes · YAML · Try it; Release among its verbs", async ({ page, isMobile }) => {
  const pane = await openPane(page, isMobile, "hyper-light/ci-kit", ".github/actions/setup-rust");
  await expect(pane.locator(".jp-g > span:first-child")).toHaveText(["Inputs", "Outputs", "Steps", "Versions", "Used by", "Settings"]);
  await expect(pane.locator(".jp-j[data-at]")).toHaveAttribute("data-key", "versions.head");
  await expect(pane.locator(".ppl-detail").getByRole("tab")).toHaveText(["Changes", "YAML", "Try it"]);
  await expect(pane.locator('.jp-g[data-group="Used by"] > span:last-child')).toHaveText("3 repositories");
  await expect(pane.locator(".ppl-verbs button")).toHaveText(["Run", "Save draft", "Ready", "Discard", "Release"]);
  await expect(pane.locator(".ppl-kicker")).toContainText(".github/actions/setup-rust · Composite action · main · e41a9c2");
  expect(await sideways(page)).toBe(0);
});

test("kiln's update draft: DRAFT on its head, what changes for its repository first, then upstream", async ({ page, isMobile }) => {
  const pane = await openPane(page, isMobile, "hyper-light/quiver", ".github/workflows/ci.yml", "d-update-88");
  await expect(pane.locator(".ppl-kicker .ppl-draft")).toHaveText("DRAFT");
  await expect(pane.locator(".jp-g > span:first-child").first()).toHaveText("What changes for it");
  await expect(pane.locator(".jp-j[data-at]")).toHaveAttribute("data-key", "changes.input targets");
  await expect(pane.locator(".ppl-detail").getByRole("tab")).toHaveText(["Changes", "YAML", "Plan"]);
  expect(await sideways(page)).toBe(0);
});

test("nothing moves as lines are picked and views switched: the list holds still, every view one height", async ({ page, isMobile }) => {
  const pane = await openPane(page, isMobile, "hyper-light/kiln", ".kiln/smoke.yaml");
  const tops = () => pane.locator(".jp-j").evaluateAll((els) => els.map((e) => Math.round(e.getBoundingClientRect().top + window.scrollY)));
  const heightOf = () => pane.locator(".ppl-dbody").evaluate((e) => e.getBoundingClientRect().height);
  const height = await heightOf();
  for (const tab of ["Plan", "Last run", "YAML"]) {
    await pane.locator(".ppl-detail").getByRole("tab", { name: tab }).click();
    expect(await heightOf()).toBe(height);
  }
  if (!isMobile) {
    // Beside the list, the detail changes nothing of it.
    const before = await tops();
    for (const key of ["on.schedule", "on.deploy", "jobs.report", "uses.kiln/notify@v1", "runs.87"]) {
      await pane.locator(`.jp-j[data-key="${key}"] .jp-line`).click();
      await expect(pane.locator(".jp-j[data-at]")).toHaveAttribute("data-key", key);
      expect(await tops()).toEqual(before);
    }
  }
  await pane.locator('.jp-j[data-key="on.schedule"] .jp-line').click();
  await expect(pane.locator(".ppl-detail").getByRole("tab")).toHaveText(["Times", "YAML", "Plan"]);
  expect(await heightOf()).toBe(height);
});
