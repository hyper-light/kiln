/**
 * Ship's Pipelines (docs/ci.md W29-R01 to R08, as Ada approved its stills on 2026-10-31), against
 * the mock's three repositories, at the stills' widths: 1280 on a desktop project, 390 on a phone's,
 * in Chromium and (E2E_WEBKIT=1) WebKit. Pipelines is Ship's third tab; its rows are Ship's by
 * repository, needs-someone first, DRAFT beside a draft's title, and a New row last; `?repo=&path=`
 * opens a row and brings it into view; nothing scrolls sideways.
 */
import { expect, test, type Locator, type Page } from "@playwright/test";

const REAL = !!process.env.E2E_BASE_URL;
test.skip(REAL, "drives the mock's pipelines");

/** The stills' widths: 1280 for a desktop, 390 for a phone. */
const atStillWidth = (page: Page, isMobile: boolean) => page.setViewportSize(isMobile ? { width: 390, height: 844 } : { width: 1280, height: 900 });

/** The app's own requests waiting on you come and go over the page's foot: set aside, as Deploys' spec does. */
const notices = (page: Page) => page.addStyleTag({ content: ".rq-viewport { display: none !important; }" });

const list = (page: Page) => page.getByRole("list", { name: "Workflows and actions by repository" });
/** A repository's rows, in the list's order, up to the next heading. */
const rowsOf = (page: Page, repo: string) => list(page).locator(`.ppl-row[data-key^="${repo}:"]:not(.ppl-new)`);

/**
 * The editor's end, its last line in view: CodeMirror draws only the lines the window shows, so the
 * editor comes into view before its scroller goes to its end.
 */
async function toTheEnd(pane: Locator) {
  await pane.locator(".ppl-detail .cm-editor").scrollIntoViewIfNeeded();
  await pane.locator(".ppl-detail .cm-scroller").evaluate((e) => (e.scrollTop = e.scrollHeight));
  await pane.page().waitForTimeout(150);
}

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
  await expect(pane.locator(".ppl-list > .jp-g > span:first-child")).toHaveText(["Triggers", "Stage 1", "Stage 2", "Uses", "Secrets and variables", "Runs", "Settings"]);
  await expect(pane.locator('.jp-g[data-group="Stage 1"] > span:last-child')).toHaveText("3 jobs at once");
  await expect(pane.locator('.jp-g[data-group="Stage 2"] > span:last-child')).toHaveText("after rust and ui");
  // The first job is picked: YAML · Plan · Last run, its lines tinted in the code.
  await expect(pane.locator(".jp-j[data-at]")).toHaveAttribute("data-key", "jobs.rust");
  // The strip says where it runs, what it reserves and its last run (pl-2).
  await expect(pane.locator(".jp-said")).toHaveText("rust picked · stage 1 · bare metal, linux · 2 CPUs, 16 GiB · its last run passed in 18m 40s on a3f2c1d");
  const detail = pane.locator(".ppl-detail");
  await expect(detail.getByRole("tab")).toHaveText(["YAML", "Plan", "Last run"]);
  // Edited in place: the job's lines tinted in the editor, opened at a line above them.
  await expect(detail.locator(".cm-line.cm-picked").first()).toContainText("rust:");
  await expect.poll(() => detail.locator(".cm-scroller").evaluate((e) => e.scrollTop)).toBeGreaterThan(0);
  // Beside the list on a desktop; right under its line on a phone.
  if (isMobile) await expect(pane.locator('.jp-j[data-key="jobs.rust"] + li.jp-detail .ppl-detail')).toBeVisible();
  else await expect(pane.locator(".ppl-side .ppl-detail")).toBeVisible();
  // A trigger: its Filters, YAML and Plan; the plan is its event's.
  await pane.locator('.jp-j[data-key="on.push"] .jp-line').click();
  await expect(detail.getByRole("tab")).toHaveText(["Filters", "YAML", "Plan"]);
  await expect(detail.locator('li[data-field="branches"] input')).toHaveValue("main");
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
  await expect(pane.locator(".ppl-list > .jp-g > span:first-child")).toHaveText(["Inputs", "Steps", "Versions", "Used by", "Settings"]);
  await expect(pane.locator(".jp-j[data-at]")).toHaveAttribute("data-key", "versions.head");
  await expect(pane.locator(".ppl-detail").getByRole("tab")).toHaveText(["Changes", "YAML", "Try it"]);
  await expect(pane.locator('.jp-g[data-group="Used by"] > span:last-child')).toHaveText("3 repositories");
  await expect(pane.locator(".ppl-verbs button")).toHaveText(["Run", "Save draft", "Ready", "Discard", "Release"]);
  await expect(pane.locator(".ppl-kicker")).toContainText("hyper-light/ci-kit · .github/actions/setup-rust · composite action");
  expect(await sideways(page)).toBe(0);
});

test("kiln's update draft: DRAFT on its head, what changes for its repository first, then upstream", async ({ page, isMobile }) => {
  const pane = await openPane(page, isMobile, "hyper-light/quiver", ".github/workflows/ci.yml", "d-update-88");
  await expect(pane.locator(".ppl-kicker .ppl-draft")).toHaveText("DRAFT");
  await expect(pane.locator(".jp-g > span:first-child").first()).toHaveText("What changes for it");
  await expect(pane.locator(".jp-j[data-at]")).toHaveAttribute("data-key", "changes.input.targets");
  await expect(pane.locator(".ppl-detail").getByRole("tab")).toHaveText(["Changes", "YAML", "Plan"]);
  // Before anything's picked, the strip says what's out and what it changes, its names in bold (pl-7).
  await expect(pane.locator(".jp-said")).toHaveText(
    "rust-check v2.4.0 is out · 3 commits since v2.3.0 · a new optional input; no new secrets, permissions or egress · kiln opened draft pull request #88",
  );
  await expect(pane.locator(".jp-said b")).toHaveText(["rust-check v2.4.0", "#88"]);
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

test("a picked schedule's strip says when it runs in its zone's words, and its next run (pl-8)", async ({ page, isMobile }) => {
  const pane = await openPane(page, isMobile, "hyper-light/kiln", ".kiln/smoke.yaml");
  await pane.locator('.jp-j[data-key="on.schedule"] .jp-line').click();
  await expect(pane.locator(".jp-said")).toHaveText(/^schedule picked · every day at 03:00 Los Angeles time · next (today|tomorrow|in \d+ days), 1[01]:00 UTC(: daylight saving (ends|starts) \w+)?$/);
  await expect(pane.locator(".jp-said b")).toHaveText("schedule");
  expect(await sideways(page)).toBe(0);
});

test("R04's pick words: a published workflow's lines narrowed to its last run's failures or a draft's changes, the rest dimmed in place (pl-2)", async ({ page, isMobile }) => {
  await page.request.get("/api/mock/ship/pipelines?reset=1");
  const dimmed = (pane: Locator) => pane.locator(".ppl-list .jp-j:not([data-dim])").evaluateAll((els) => els.map((e) => e.getAttribute("data-key")));
  // ci: the three words, all picked; its last run passed, so failed last run dims every line and says why.
  const ci = await openPane(page, isMobile, "hyper-light/kiln", ".kiln/ci.yaml");
  const pick = ci.locator(".jp-q");
  await expect(pick).toHaveText(/^pick:\s*failed last run\s*·\s*changed in a draft\s*·\s*all$/);
  await expect(pick.locator("button[aria-pressed=true]")).toHaveText("all");
  await expect(ci.locator(".ppl-list .jp-j[data-dim]")).toHaveCount(0);
  const tops = () => ci.locator(".jp-j").evaluateAll((els) => els.map((e) => Math.round(e.getBoundingClientRect().top + window.scrollY)));
  const before = await tops();
  await pick.getByRole("button", { name: "failed last run" }).click();
  await expect(pick.locator("button[aria-pressed=true]")).toHaveText("failed last run");
  await expect(ci.locator(".jp-said")).toHaveText("Nothing failed in its last run.");
  expect(await dimmed(ci)).toEqual([]);
  // Dimmed, never taken out: every line where it was.
  expect(await tops()).toEqual(before);
  await pick.getByRole("button", { name: "all" }).click();
  await expect(ci.locator(".ppl-list .jp-j[data-dim]")).toHaveCount(0);
  // e2e: its last run failed its e2e job, and only that line stays lit.
  const e2e = await openPane(page, isMobile, "hyper-light/kiln", ".kiln/e2e.yaml");
  await e2e.locator(".jp-q").getByRole("button", { name: "failed last run" }).click();
  await expect.poll(() => dimmed(e2e)).toEqual(["jobs.e2e"]);
  // quiver's ci: kiln's update draft moves rust-check, so the job that calls it and the use stay lit.
  const quiver = await openPane(page, isMobile, "hyper-light/quiver", ".github/workflows/ci.yml");
  await quiver.locator(".jp-q").getByRole("button", { name: "changed in a draft" }).click();
  await expect.poll(() => dimmed(quiver)).toEqual(["jobs.rust", "uses.hyper-light/ci-kit/.github/workflows/rust-check.yml@v2.3.0"]);
  expect(await sideways(page)).toBe(0);
});

test("plan for: takes the pick words' place when a trigger is picked, and lists what the preview plans without a payload (pl-8)", async ({ page, isMobile }) => {
  const pane = await openPane(page, isMobile, "hyper-light/kiln", ".kiln/smoke.yaml");
  await pane.locator('.jp-j[data-key="on.schedule"] .jp-line').click();
  const q = pane.locator(".jp-q");
  // Not webhook's or comment's: what they carry is what starts them.
  await expect(q).toHaveText(/^plan for:\s*deploy\s*·\s*schedule\s*·\s*pipeline\s*·\s*manual$/);
  await expect(q.locator("button[aria-pressed=true]")).toHaveText("schedule");
  // A job picked again: the pick words.
  await pane.locator('.jp-j[data-key="jobs.smoke"] .jp-line').click();
  await expect(q).toHaveText(/^pick:/);
});

test("no layout shift as lines are picked, views switched, a verb asked and kept, and the pane refreshed", async ({ page, isMobile, browserName }) => {
  test.skip(browserName !== "chromium", "layout shifts are Chromium's to count");
  test.setTimeout(60_000);
  await page.request.get("/api/mock/ship/pipelines?reset=1");
  const pane = await openPane(page, isMobile, "hyper-light/kiln", ".kiln/smoke.yaml");
  await page.evaluate(() => document.fonts.ready.then(() => undefined));
  await page.evaluate(() => {
    const w = window as unknown as { __shift: number; __moved: string[] };
    w.__shift = 0;
    w.__moved = [];
    new PerformanceObserver((l) => {
      for (const e of l.getEntries() as unknown as { value: number; hadRecentInput: boolean; sources?: { node?: Node }[] }[]) {
        if (e.hadRecentInput) continue;
        if (!(e.sources ?? []).some((s) => (s.node instanceof Element ? s.node : (s.node?.parentElement ?? null))?.closest(".ship"))) continue;
        w.__shift += e.value;
        for (const s of e.sources ?? []) w.__moved.push(`${s.node?.nodeName ?? "?"} "${(s.node?.textContent ?? "").slice(0, 40)}"`);
      }
    }).observe({ type: "layout-shift" });
  });
  for (const k of ["jobs.report", "on.schedule", "on.deploy", "add-trigger", "jobs.smoke"]) {
    await pane.locator(`.jp-j[data-key="${k}"] .jp-line`).click();
    await page.waitForTimeout(600);
  }
  for (const v of ["Plan", "Last run", "YAML"]) {
    await pane.locator(".ppl-detail").getByRole("tab", { name: v }).click();
    await page.waitForTimeout(600);
  }
  // The pick words narrow the list and widen it again (R04): dimmed in place, nothing moves.
  for (const w of ["failed last run", "changed in a draft", "all"]) {
    await pane.locator(".jp-q").getByRole("button", { name: w }).click();
    await page.waitForTimeout(300);
  }
  // Run asks for its inputs in the strip; Keep goes back.
  await pane.locator(".ppl-verbs button", { hasText: /^Run$/ }).click();
  await page.waitForTimeout(300);
  await pane.locator(".ppl-verbs button", { hasText: "Keep" }).click();
  // The server says the repository's rows changed: the list and the pane ask again.
  for (let i = 0; i < 2; i++) {
    await page.request.post("/api/mock/ship/pipelines/tick?repo=hyper-light/kiln");
    await page.waitForTimeout(2_000);
  }
  const seen = await page.evaluate(() => ({ shift: (window as unknown as { __shift: number }).__shift, moved: (window as unknown as { __moved: string[] }).__moved }));
  expect(seen.shift, `what moved: ${seen.moved.join(" | ")}`).toBe(0);
});

for (const width of [320, 390, 1280]) {
  test(`at ${width}, an opened workflow scrolls nothing sideways, whatever its detail shows`, async ({ page, isMobile }) => {
    test.skip(isMobile, "each width is set here; the desktop projects run them");
    await page.request.get("/api/mock/ship/pipelines?reset=1");
    await page.setViewportSize({ width, height: 900 });
    await page.goto(`/ship/pipelines?repo=${encodeURIComponent("hyper-light/kiln")}&path=${encodeURIComponent(".kiln/smoke.yaml")}`);
    await notices(page);
    const pane = page.getByRole("region", { name: "Pipeline" });
    await expect(pane.locator(".jp-strip")).toBeVisible({ timeout: 15_000 });
    const within = async (what: string) => {
      expect(await sideways(page), what).toBe(0);
      for (const sel of [".jp-strip", ".ppl-verbs button", ".jp-list", ".jp-j", ".ppl-detail"]) {
        const right = await page.locator(sel).evaluateAll((els) => Math.max(0, ...els.map((n) => n.getBoundingClientRect().right)));
        expect(right, `${what}: ${sel}`).toBeLessThanOrEqual(width + 0.5);
      }
    };
    await within("its first job");
    for (const k of ["on.schedule", "on.webhook", "add-trigger", "jobs.report"]) {
      await pane.locator(`.jp-j[data-key="${k}"] .jp-line`).click();
      await within(k);
    }
    await pane.locator(".ppl-detail").getByRole("tab", { name: "Plan" }).click();
    await within("its plan");
    await pane.locator(".ppl-verbs button", { hasText: /^Run$/ }).click();
    await within("Run's inputs asked");
  });
}

test("the file is edited where it's shown: kiln's colours, run: as bash; typing starts a draft; its problems under it with their fixes; nothing floats", async ({ page, isMobile }) => {
  const pane = await openPane(page, isMobile, "hyper-light/kiln", ".kiln/ci.yaml");
  const code = pane.locator(".ppl-detail .cm-content");
  await expect(code).toBeVisible();
  // kiln's tree-sitter colours: keys as properties, a run: line as bash (its command a function).
  await expect(pane.locator(".ppl-detail .cm-line .hl-prop", { hasText: "runs-on" }).first()).toBeVisible();
  await expect(pane.locator(".ppl-detail .cm-line .hl-fn", { hasText: "cargo" }).first()).toBeVisible();
  // A job typed at the end, its use following a tag (the end reached as a person would, on any platform).
  await toTheEnd(pane);
  await pane.locator(".ppl-detail .cm-line").last().click();
  await page.keyboard.press("End");
  await page.keyboard.insertText("\n  helm-lint:\n    runs-on: [baremetal, linux]\n    steps:\n      - uses: azure/setup-helm@v4\n");
  // Not saved: the row and the head say so, and Save draft is on.
  const row = page.locator('.ppl-row[data-key="hyper-light/kiln:.kiln/ci.yaml"]');
  await expect(row.locator(".ship-r .ship-stand")).toHaveText("Not saved");
  await expect(pane.locator(".ppl-kicker .ppl-draft")).toBeVisible();
  // What it adds and what its checks found, said in the strip; the row says how much it changes and its plan against main's;
  // its job and its use join their groups, marked new (pl-3).
  await expect(pane.locator(".jp-said")).toHaveText("Adds helm-lint · checks clean but for 1 warning · not saved yet · a draft runs on no trigger until it lands");
  await expect(row.locator(".ship-r .ship-text")).toHaveText("yours · 1 change · plans 6 jobs (was 5)");
  await expect(pane.locator('.jp-j[data-key="jobs.helm-lint"]')).toContainText("new job");
  await expect(pane.locator('.jp-g[data-group="Stage 1"] > span:last-child')).toHaveText("4 jobs at once · 1 new");
  await expect(pane.locator('.jp-j[data-key="uses.azure/setup-helm@v4"]')).toContainText("pin it?");
  await expect(pane.locator(".ppl-verbs button", { hasText: "Save draft" })).not.toHaveAttribute("data-dim");
  // Its problems sit under the code once typing stops: the pin, with its fix.
  const problems = pane.getByLabel("Problems");
  await expect(problems.locator("li")).toHaveCount(1);
  await expect(problems.locator("li")).toContainText("KCI6002");
  await problems.getByRole("button", { name: "Pin it to 1a275c3" }).click();
  await expect(code).toContainText("azure/setup-helm@1a275c3f0b9e8d7c6b5a4f3e2d1c0b9a8f7e6d5c");
  await expect(problems.locator("li")).toHaveCount(0);
  await expect(pane.locator(".jp-said")).toContainText("Adds helm-lint · checks clean · not saved yet");
  // Nothing floats over the page, hovering the code or not.
  await pane.locator(".ppl-detail .cm-line", { hasText: "helm-lint" }).hover();
  await expect(page.locator(".cm-tooltip, [role=dialog]")).toHaveCount(0);
  expect(await sideways(page)).toBe(0);
});

test("the plan for each event: plan for: picks it, a picked job says where it would run, a draft's plan says what changes", async ({ page, isMobile }) => {
  const pane = await openPane(page, isMobile, "hyper-light/kiln", ".kiln/ci.yaml");
  // A job picked: the pick words (pl-2), and its machines under its code.
  const planFor = pane.locator(".jp-q");
  await expect(planFor).toHaveText(/pick:\s*failed last run\s*·\s*changed in a draft\s*·\s*all/);
  const under = pane.locator(".ppl-problems");
  await expect(under).toContainText("Where it would run");
  await expect(under).toContainText("fedora, adacachy can take it now: baremetal, linux");
  await expect(under).toContainText("2 of 2");
  // A trigger picked: its triggers, by event, its own picked.
  await pane.locator('.jp-j[data-key="on.push"] .jp-line').click();
  await expect(planFor).toHaveText(/plan for:\s*push to main\s*·\s*pull request\s*·\s*manual/);
  await expect(planFor.locator("button[aria-pressed=true]")).toHaveText("push to main");
  // Another event: the Plan view asks for it.
  await planFor.getByRole("button", { name: "pull request" }).click();
  await expect(planFor.locator("button[aria-pressed=true]")).toHaveText("pull request");
  const asked = page.waitForRequest((r) => r.url().endsWith("/api/ci/plan-preview") && (r.postDataJSON() as { event?: unknown }).event === "pull_request");
  await pane.locator(".ppl-detail").getByRole("tab", { name: "Plan" }).click();
  await asked;
  await expect(pane.locator(".ppl-detail .ppl-lines li")).toHaveCount(5);
  // A job typed in: the plan for a push to main says what's new, and the rest as on main (pl-3).
  await planFor.getByRole("button", { name: "push to main" }).click();
  await pane.locator(".ppl-detail").getByRole("tab", { name: "YAML" }).click();
  await toTheEnd(pane);
  await pane.locator(".ppl-detail .cm-line").last().click();
  await page.keyboard.press("End");
  await page.keyboard.insertText("\n  helm-lint:\n    runs-on: [baremetal, linux]\n    steps:\n      - uses: kiln/checkout@v1\n");
  await expect(under).toContainText("Plan for a push to main");
  await expect(under.locator("li[data-mark=added]")).toHaveText(/helm-lint, a new job: 1 leg, on fedora or adacachy \(baremetal, linux\)\s*\+1 job/);
  await expect(under.locator("li[data-mark=same]")).toContainText("the other 5 jobs and 7 legs as on main");
  // The Plan view says the same, against what's published.
  await pane.locator(".ppl-detail").getByRole("tab", { name: "Plan" }).click();
  await expect(pane.locator(".ppl-detail .ppl-lines li[data-mark=added]")).toContainText("helm-lint, a new job");
  expect(await sideways(page)).toBe(0);
});

test("Add a trigger: 18 kinds in six groups, shown by whose; picking one adds its lines as a draft and opens its filters; a schedule's times and options", async ({ page, isMobile }) => {
  const pane = await openPane(page, isMobile, "hyper-light/kiln", ".kiln/smoke.yaml");
  await pane.locator('.jp-j[data-key="add-trigger"] .jp-line').click();
  const kinds = pane.getByLabel("Add a trigger");
  await expect(kinds.locator("li[data-kind]")).toHaveCount(18);
  await expect(kinds.locator(".jp-g > span:first-child")).toHaveText(["Code", "Time", "People and agents", "Other workflows", "Deploys and releases", "Outside kiln"]);
  await expect(pane.locator(".jp-said")).toContainText("pick what starts it");
  // show: kiln's lists kiln's own and both; GitHub's the rest and both.
  const show = pane.locator(".jp-q");
  await show.getByRole("button", { name: "kiln's" }).click();
  await expect(kinds.locator("li[data-kind]")).toHaveCount(10);
  await show.getByRole("button", { name: "GitHub's" }).click();
  await expect(kinds.locator("li[data-kind]")).toHaveCount(12);
  await show.getByRole("button", { name: "all" }).click();
  // One it has already: dimmed, saying so.
  await kinds.locator('li[data-kind="schedule"] button').click({ force: true });
  await expect(pane.locator(".jp-said")).toContainText("it has a schedule trigger already");
  // A push: its lines added, Not saved, its filters open under it.
  await kinds.locator('li[data-kind="push"] button').click();
  await expect(page.locator('.ppl-row[data-key="hyper-light/kiln:.kiln/smoke.yaml"] .ship-r .ship-stand')).toHaveText("Not saved");
  await expect(pane.locator(".jp-j[data-at]")).toHaveAttribute("data-key", "on.push");
  const filters = pane.getByLabel("push's filters");
  await expect(filters.locator("li[data-field]")).toHaveCount(6);
  await expect(filters.locator('li[data-field="branches"] input')).toHaveValue("main");
  // A filter typed: the YAML gains it.
  await filters.locator('li[data-field="tags"] input').fill("v*, release-*");
  await filters.locator('li[data-field="tags"] input').press("Enter");
  await expect(filters.locator('li[data-field="tags"] input')).toHaveValue("v*, release-*");
  await pane.locator(".ppl-detail").getByRole("tab", { name: "YAML" }).click();
  await expect(pane.locator(".ppl-detail .cm-content")).toContainText("release-*");
  // The schedule: its next three runs in its zone and UTC, and its options, each changed where it's shown.
  await pane.locator('.jp-j[data-key="on.schedule"] .jp-line').click();
  const times = pane.getByLabel("0 3 * * *'s next runs");
  await expect(times.locator("li")).toHaveCount(3);
  await expect(times.locator("li").first()).toContainText(/03:00 P[SD]T · 1[01]:00 UTC/);
  const options = pane.getByLabel("0 3 * * *'s options");
  await expect(options.locator('li[data-field="skip-if-unchanged"] [role=switch]')).toHaveAttribute("aria-checked", "true");
  await options.locator('li[data-field="catch-up"] [role=switch]').click();
  await expect(pane.getByLabel("0 3 * * *'s options").locator('li[data-field="catch-up"] [role=switch]')).toHaveAttribute("aria-checked", "true");
  await expect(page.locator(".cm-tooltip, [role=dialog]")).toHaveCount(0);
  expect(await sideways(page)).toBe(0);
});

/** The bodies this browser sent `POST /api/ship/pipelines`, as the mock kept them. */
const sentBodies = async (page: Page) => ((await (await page.request.get("/api/mock/ship/pipelines")).json()) as { bodies: Record<string, unknown>[] }).bodies;

test("a GitHub workflow keeps working on GitHub: a schedule goes in its YAML, kiln's diff and the schedule's zone in its overlay; plan for: lists them all; Save draft carries both (W29-S03)", async ({
  page,
  isMobile,
}) => {
  await page.request.get("/api/mock/ship/pipelines?reset=1");
  const pane = await openPane(page, isMobile, "hyper-light/quiver", ".github/workflows/ci.yml");
  const kinds = pane.getByLabel("Add a trigger");
  const add = async (kind: string) => {
    await pane.locator('.jp-j[data-key="add-trigger"] .jp-line').click();
    await kinds.locator(`li[data-kind="${kind}"] button`).click();
  };
  // A schedule, GitHub's too: into the workflow's YAML.
  await add("schedule");
  await expect(pane.locator(".jp-j[data-at]")).toHaveAttribute("data-key", "on.schedule");
  await expect(page.locator('.ppl-row[data-key="hyper-light/quiver:.github/workflows/ci.yml"] .ship-r .ship-stand')).toHaveText("Not saved");
  // kiln's diff: into its overlay, its filters the engine's, its YAML the overlay's.
  await add("diff");
  const diff = pane.locator('.jp-j[data-key="on.diff"]');
  await expect(pane.locator(".jp-j[data-at]")).toHaveAttribute("data-key", "on.diff");
  await expect(diff).toContainText("onto main");
  await expect(diff).toContainText("in its overlay");
  await expect(pane.locator(".ppl-dname")).toHaveText("onto main · .kiln/overlays/ci.yaml");
  const filters = pane.getByLabel("diff's filters");
  await expect(filters.locator("li[data-field] > .n")).toHaveText(["branches", "paths", "paths-ignore", "drafts", "authors"]);
  await expect(filters.locator('li[data-field="drafts"] [role=switch]')).toHaveAttribute("aria-checked", "false");
  await pane.locator(".ppl-detail").getByRole("tab", { name: "YAML" }).click();
  await expect(pane.locator(".ppl-detail .cm-content")).toContainText("# kiln's own keys for .github/workflows/ci.yml");
  await expect(pane.locator(".ppl-detail .cm-content")).toContainText("diff:");
  // plan for: lists GitHub's and kiln's alike.
  await expect(pane.locator(".jp-q")).toHaveText(/plan for:\s*push to main\s*·\s*pull request\s*·\s*schedule\s*·\s*diff/);
  // The schedule's zone is kiln's key: it goes in the overlay, on the entry with its cron.
  await pane.locator('.jp-j[data-key="on.schedule"] .jp-line').click();
  const zone = pane.getByLabel("0 3 * * *'s options").locator('li[data-field="timezone"] input');
  await zone.fill("America/Los_Angeles");
  await zone.press("Enter");
  await expect(pane.getByLabel("0 3 * * *'s next runs").locator("li").first()).toContainText(/03:00 P[SD]T · 1[01]:00 UTC/);
  // Saved: the workflow with its schedule and nothing of kiln's; its overlay beside it.
  await pane.locator(".ppl-verbs button", { hasText: "Save draft" }).click();
  await expect(pane.locator(".jp-said")).toContainText("as a draft");
  const body = (await sentBodies(page)).at(-1) as { verb: string; content: string; files?: { path: string; text: string }[] };
  expect(body.verb).toBe("save_draft");
  expect(body.content).toContain("schedule:");
  expect(body.content).not.toMatch(/diff|timezone/);
  expect(body.files?.map((f) => f.path)).toEqual([".kiln/overlays/ci.yaml"]);
  expect(body.files?.[0]?.text).toMatch(/diff:[\s\S]*schedule:[\s\S]*cron: "?0 3 \* \* \*"?[\s\S]*timezone: America\/Los_Angeles/);
  await expect(page.locator(".cm-tooltip, [role=dialog]")).toHaveCount(0);
  expect(await sideways(page)).toBe(0);
});

test("Run with inputs: the strip asks for them in place, each its default first; one with none runs at once; nothing moves", async ({ page, isMobile }) => {
  await page.request.get("/api/mock/ship/pipelines?reset=1");
  const pane = await openPane(page, isMobile, "hyper-light/kiln", ".kiln/smoke.yaml");
  const strip = pane.locator(".jp-strip");
  const height = await strip.evaluate((e) => e.getBoundingClientRect().height);
  await pane.locator(".ppl-verbs button", { hasText: "Run" }).click();
  // Its manual trigger's input, its default picked, asked where the strip's words were.
  const inputs = pane.getByLabel("Its inputs");
  await expect(inputs.locator('[data-input="target"] button[aria-pressed=true]')).toHaveText("staging");
  await expect(pane.locator(".jp-said")).toContainText("by hand, with its inputs");
  await expect(pane.locator(".ppl-verbs button")).toHaveText(["Keep", "Run"]);
  expect(await strip.evaluate((e) => e.getBoundingClientRect().height)).toBe(height);
  await inputs.locator('[data-input="target"]').getByRole("button", { name: "production" }).click();
  await pane.locator(".ppl-verbs button", { hasText: "Run" }).click();
  await expect(pane.locator(".jp-said")).toContainText("Running smoke by hand with target production");
  expect((await sentBodies(page)).at(-1)).toEqual({ repo: "hyper-light/kiln", path: ".kiln/smoke.yaml", verb: "run", inputs: { target: "production" } });
  await expect(page.locator("[role=dialog]")).toHaveCount(0);
  // ci has no inputs: Run runs it at once.
  const ci = await openPane(page, isMobile, "hyper-light/kiln", ".kiln/ci.yaml");
  await ci.locator(".ppl-verbs button", { hasText: "Run" }).click();
  await expect(ci.locator(".jp-said")).toContainText("Running ci by hand");
  expect((await sentBodies(page)).at(-1)).toEqual({ repo: "hyper-light/kiln", path: ".kiln/ci.yaml", verb: "run" });
  expect(await sideways(page)).toBe(0);
});

test("New: seven kinds, the dialect a workflow's, where it starts; the files it makes edited in place; Create draft saves them as a draft", async ({ page, isMobile }) => {
  await page.request.get("/api/mock/ship/pipelines?reset=1");
  await atStillWidth(page, isMobile);
  await page.goto("/ship/pipelines");
  await notices(page);
  const row = page.locator('.ppl-row[data-key="hyper-light/ci-kit:new"]');
  await row.locator(".ship-main").click();
  const pane = row.getByRole("region", { name: "New" });
  await expect(pane.locator('.jp-j[data-key^="make."]')).toHaveCount(7);
  await expect(pane.locator('.jp-j[data-key="lang.ts"]')).toHaveAttribute("data-dim", "true");
  // A Firecracker action: its three files, its path under its name.
  await pane.locator('.jp-j[data-key="make.firecracker"] .jp-line').click();
  await expect(pane.locator(".jp-said")).toContainText("Firecracker action");
  const tabs = pane.getByRole("tablist", { name: "Its files" }).getByRole("tab");
  await expect(tabs).toHaveText(["action.yml", "Dockerfile", "run.sh"]);
  const name = pane.getByLabel("Name", { exact: true });
  await name.fill("fuzz-more");
  await name.press("Enter");
  await expect(pane.locator('li[data-field="path"] .v')).toHaveText(".github/actions/fuzz-more/action.yml");
  // A dialect is a workflow's: dimmed for an action, saying why.
  await pane.locator('.jp-j[data-key="lang.kiln"] .jp-line').click({ force: true });
  await expect(pane.locator(".jp-said")).toContainText("An action is its action.yml");
  // run.sh edited where it's shown, then saved with the rest.
  await pane.getByRole("tablist", { name: "Its files" }).getByRole("tab", { name: "run.sh" }).click();
  await pane.locator(".ppl-newcode .cm-line").last().click();
  await page.keyboard.press("End");
  await page.keyboard.insertText("\necho done");
  await pane.locator(".ppl-verbs button", { hasText: "Create draft" }).click();
  await expect(pane.locator(".jp-said")).toContainText("Saved fuzz-more as a draft");
  const body = (await sentBodies(page)).at(-1) as { files?: { path: string; text: string }[] } & Record<string, unknown>;
  expect(body).toMatchObject({ repo: "hyper-light/ci-kit", verb: "new", kind: "firecracker", name: "fuzz-more" });
  expect(body.files?.map((f) => f.path)).toEqual([".github/actions/fuzz-more/action.yml", ".github/actions/fuzz-more/Dockerfile", ".github/actions/fuzz-more/run.sh"]);
  expect(body.files?.find((f) => f.path.endsWith("run.sh"))?.text).toContain("echo done");
  // Its row: DRAFT, Unpublished, in its repository.
  const made = page.locator('.ppl-row[data-key^="hyper-light/ci-kit:.github/actions/fuzz-more/action.yml:draft:"]');
  await expect(made.locator(".ship-r .ship-stand")).toHaveText("Unpublished");
  await expect(made.locator(".ppl-draft")).toHaveText("DRAFT");
  // A workflow in kiln's dialect: its path under .kiln/.
  await pane.locator('.jp-j[data-key="make.workflow"] .jp-line').click();
  await pane.locator('.jp-j[data-key="lang.kiln"] .jp-line').click();
  await expect(pane.locator('li[data-field="path"] .v')).toHaveText(".kiln/new-workflow.yaml");
  await expect(page.locator("[role=dialog], .cm-tooltip")).toHaveCount(0);
  expect(await sideways(page)).toBe(0);
});

test("Release asked in the strip: the version what it changes calls for, the others beside it; Keep sends nothing; a refusal over checks asks Release anyway", async ({ page, isMobile }) => {
  await page.request.get("/api/mock/ship/pipelines?reset=1");
  const pane = await openPane(page, isMobile, "hyper-light/ci-kit", ".github/actions/setup-rust");
  // Its Changes: the commits since its newest tag, and what releasing changes for those who use it.
  const changes = pane.locator(".ppl-detail");
  await expect(changes.getByLabel("Commits since v2.4.0").locator("li")).toHaveCount(3);
  await expect(changes).toContainText("What v2.5.0 changes for those who use it");
  const release = () => pane.locator(".ppl-verbs button", { hasText: /^Release/ });
  await release().click();
  await expect(pane.locator(".jp-said")).toContainText("Release v2.5.0 at e41a9c2, main's tip? The tag is ci-kit's: its 5 actions and its workflow all get v2.5.0.");
  const as = pane.locator(".jp-q");
  await expect(as.locator("button")).toHaveText(["v2.5.0", "v2.4.1", "v3.0.0"]);
  await expect(pane.locator(".ppl-verbs button")).toHaveText(["Keep", "Release"]);
  // Keep: back, nothing sent.
  await pane.locator(".ppl-verbs button", { hasText: "Keep" }).click();
  await expect(pane.locator(".ppl-verbs button")).toHaveText(["Run", "Save draft", "Ready", "Discard", "Release"]);
  expect(await sentBodies(page)).toEqual([]);
  // Another version picked, released: its major's floating tag moves too.
  await release().click();
  await as.getByRole("button", { name: "v2.4.1" }).click();
  await expect(pane.locator(".jp-said")).toContainText("Release v2.4.1 at e41a9c2");
  await release().click();
  await expect(pane.locator(".jp-said")).toContainText("Released v2.4.1 at e41a9c2");
  expect((await sentBodies(page)).at(-1)).toMatchObject({ verb: "release", path: ".github/actions/setup-rust", tag: "v2.4.1", major: true, sha: "e41a9c2b7d6e5f4a3b2c1d0e9f8a7b6c5d4e3f21" });
  // cache-cargo's checks failed: the refusal says why, and asks Release anyway.
  const cargo = await openPane(page, isMobile, "hyper-light/ci-kit", ".github/actions/cache-cargo");
  await cargo.locator(".ppl-verbs button", { hasText: /^Release/ }).click();
  await cargo.locator(".ppl-verbs button", { hasText: /^Release/ }).click();
  await expect(cargo.locator(".jp-said")).toContainText("ci-kit's checks failed on e41a9c2: rust-check · clippy. Release v2.5.0 anyway?");
  await expect(cargo.locator(".ppl-verbs button")).toHaveText(["Keep", "Release anyway"]);
  await cargo.locator(".ppl-verbs button", { hasText: "Release anyway" }).click();
  await expect(cargo.locator(".jp-said")).toContainText("Released v2.5.0");
  expect((await sentBodies(page)).at(-1)).toMatchObject({ verb: "release", tag: "v2.5.0", confirm: true });
  await expect(page.locator("[role=dialog]")).toHaveCount(0);
  expect(await sideways(page)).toBe(0);
});

test("kiln's update draft: its lines against what's published, what it changes for its repository; Run, Ready and Discard name it by its id", async ({ page, isMobile }) => {
  await page.request.get("/api/mock/ship/pipelines?reset=1");
  const pane = await openPane(page, isMobile, "hyper-light/quiver", ".github/workflows/ci.yml", "d-update-88");
  const diff = pane.getByLabel("Its lines against what's published");
  await expect(diff.locator('li[data-kind="del"]')).toContainText("rust-check.yml@v2.3.0");
  await expect(diff.locator('li[data-kind="add"]')).toContainText("rust-check.yml@v2.4.0");
  // What the new version changes for quiver, in sentences, each marked; its plan against main's; its draft's run (pl-7).
  await expect(pane.locator(".ppl-detail")).toContainText("What v2.4.0 changes for quiver");
  const changes = pane.locator(".ppl-changes .ppl-lines li");
  await expect(changes).toHaveText([
    /^a new input, targets \(optional\): quiver passes none, so nothing changes\s*safe$/,
    /^no new secrets, permissions or egress\s*same$/,
    /^the plan for a push to main: the same \d+ jobs?, \d+ legs?\s*same$/,
    /^#88's run on its draft: rust \/ check running, bench queued\s*4m$/,
  ]);
  await expect(changes.first().locator("b")).toHaveText("targets");
  await expect(pane.locator(".ppl-verbs button[data-dim]")).toHaveText(["Save draft"]);
  await pane.locator(".ppl-verbs button", { hasText: "Ready" }).click();
  await expect(pane.locator(".jp-said")).toContainText("Asked for ci's review");
  expect((await sentBodies(page)).at(-1)).toEqual({ repo: "hyper-light/quiver", path: ".github/workflows/ci.yml", verb: "ready", draft: "d-update-88" });
  expect(await sideways(page)).toBe(0);
});
