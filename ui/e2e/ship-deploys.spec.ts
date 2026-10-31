/**
 * Ship's Deploys (docs/ci.md W26-R24, as Ada approved it on 2026-10-30, stills v5 and v6), against
 * the mock's three projects: Ship's tabs are Commits and Deploys (/runs lands on Ship, /deployments
 * on Deploys); apps are Ship's rows by project, needs-you first, with no buttons; an opened app is
 * the jobs pane's anatomy (the strip, its objects by owner, its rollout, history and settings, the
 * picked one's detail beside them or under its line with the strip again on a phone). Each action
 * reaches `POST /api/ship/deploys` with the right body, Refresh, Roll back, Stop, Abort and Delete ask
 * again, a refusal says why in the strip; nothing moves, and nothing scrolls sideways.
 */
import { expect, test, type Locator, type Page } from "@playwright/test";

const REAL = !!process.env.E2E_BASE_URL;
test.skip(REAL, "drives the mock's apps");

const KILN = "default/kiln-production";

/** The app's own requests waiting on you (the mock's agents ask on their own) come and go over the page's foot,
 * and on a phone a tap meant for what's under them lands on them: they're set aside, as the layout-shift
 * counts already leave them out. */
const notices = (page: Page) => page.addStyleTag({ content: ".rq-viewport { display: none !important; }" });

/** The Deploys tab with `app` opened: its pane. */
async function open(page: Page, app = KILN): Promise<Locator> {
  await page.request.post("/api/mock/ship/deploys?reset=1");
  await page.goto(`/ship/deploys?app=${encodeURIComponent(app)}`);
  await notices(page);
  const pane = page.locator(`.dp-row[data-app="${app}"]`).getByRole("region", { name: "Deploys" });
  await expect(pane.locator(".jp-strip").first()).toBeVisible({ timeout: 15_000 });
  return pane;
}

const strip = (pane: Locator) => pane.locator(".jp-strip").first();
const verb = (pane: Locator, name: string | RegExp) => strip(pane).locator(".jp-verbs button", { hasText: name });
const verbs = (pane: Locator) => strip(pane).locator(".jp-verbs button");
const said = (pane: Locator) => strip(pane).locator(".jp-said");
/** A line of the pane, by its key (`o:` an object's, `w:` a workload's head, `h:` a history line, `s:` a step, `c:` a setting). */
const line = (pane: Locator, key: string) => pane.locator(`[data-key="${key}"]`).first();
const pickLine = (pane: Locator, key: string) => line(pane, key).locator(".jp-line").click();

/** The bodies this browser sent `POST /api/ship/deploys`, as the mock kept them. */
const sent = async (page: Page) => ((await (await page.request.get("/api/mock/ship/deploys")).json()) as { bodies: Record<string, unknown>[] }).bodies;
const last = async (page: Page) => (await sent(page)).at(-1);

test("Ship's tabs are Commits and Deploys; /runs lands on Ship, /deployments on Deploys", async ({ page }) => {
  await page.goto("/ship");
  const views = page.getByRole("navigation", { name: "Ship views" });
  await expect(views.getByRole("link")).toHaveText(["Commits", "Deploys"]);
  await expect(views.getByRole("link", { name: "Commits" })).toHaveClass(/is-current/);
  await page.goto("/runs");
  await expect(page).toHaveURL(/\/ship$/);
  await page.goto("/deployments");
  await expect(page).toHaveURL(/\/ship\/deploys$/);
  await expect(page.getByRole("navigation", { name: "Ship views" }).getByRole("link", { name: "Deploys" })).toHaveClass(/is-current/);
  await expect(page.getByRole("heading", { name: "Ship.", exact: true })).toBeVisible();
});

test("apps are Ship's rows by project, needs-you first: the stand word, the line, Sync · Health · Soak, and no buttons", async ({ page }) => {
  await page.goto("/ship/deploys");
  await notices(page);
  // What waits on a person first, then what went back, then what's out of sync: as the stills order them.
  await expect(page.locator(".dp-proj > span:first-child")).toHaveText(["hyper-light/quiver", "hyper-light/hyperlight-site", "hyper-light/kiln"]);
  await expect(page.locator(".dp-proj > span:last-child")).toHaveText(["2 apps · 1 waits on you", "1 app · rolled back", "3 apps · 1 out of sync"]);
  await expect(page.locator(".dp-row .ship-name")).toHaveText(["quiver-production", "quiver-staging", "site-production", "kiln-production", "kiln-staging", "kube-prometheus-stack-production"]);
  await expect(page.locator(".dp-row .ship-stand")).toHaveText([/Needs you/i, /Syncing/i, /Rolled back/i, /Out of sync/i, /Synced/i, /Synced/i]);
  const kiln = page.locator(`.dp-row[data-app="${KILN}"]`);
  await expect(kiln).toHaveAttribute("data-stand", "needs_you");
  await expect(kiln.locator(".ship-text")).toHaveText("#153 runs 15878aa · trunk is 2 ahead · healthy");
  await expect(kiln.locator(".js-l")).toHaveText(["Sync", "Health", "Soak"]);
  await expect(kiln.locator(".ship-env")).toHaveText("production");
  await expect(kiln.locator(".ship-sha")).toHaveText("15878aa");
  // Who and when: the when whole, in the fewest words, however long the who.
  await expect(kiln.locator(".dp-by")).toHaveText(/^agent 01a18b08\s*·\s*36m$/);
  for (const by of await page.locator(".dp-by").all()) {
    const fit = await by.evaluate((el) => {
      const ago = el.querySelector(".dp-ago");
      return !!ago && ago.scrollWidth <= ago.clientWidth && ago.getBoundingClientRect().right <= el.getBoundingClientRect().right + 0.5;
    });
    expect(fit).toBe(true);
  }
  await expect(page.locator('.dp-row[data-app="default/site-production"] .ship-text')).toHaveText("#19 failed its health check · back on #18");
  // Nothing on a row is a button but to open it.
  for (const row of await page.locator(".dp-row").all()) {
    await expect(row.locator(".ship-r button")).toHaveCount(2);
    await expect(row.locator(".ship-r button.ship-main, .ship-r button.ship-go")).toHaveCount(2);
  }
  // Finding by an environment narrows to it.
  await page.getByPlaceholder("Project, app, environment, cluster…").fill("staging");
  await expect(page.locator(".dp-row .ship-name")).toHaveText(["quiver-staging", "kiln-staging"]);
});

test("an opened app is the jobs pane's anatomy: the strip, its objects by owner, history and settings, the picked one beside them or under its line", async ({ page, isMobile }) => {
  const pane = await open(page);
  await expect(verbs(pane)).toHaveText(["Sync", "Refresh", "Roll back", "Stop"]);
  await expect(verb(pane, "Stop")).toBeDisabled();
  const widths = await verbs(pane).evaluateAll((bs) => bs.map((b) => Math.round(b.getBoundingClientRect().width)));
  expect(new Set(widths).size).toBe(1);
  await expect(strip(pane).locator(".jp-q button")).toHaveText(["out of sync", "unhealthy", "all"]);
  await expect(pane.locator(".jp-g > span:first-child, .jp-g .jp-line > span:first-child")).toHaveText(["Deployment kiln", "Network", "Config", "Storage", "Jobs", "History", "Settings"]);
  // The pod's shown first, where it runs: its log in the commits' Output's frame, by its container.
  await expect(said(pane)).toContainText("Pod kiln-cd9dc7b7-rsmwz picked · running on kiln-m02 · up 3m · 4 restarts");
  const at = pane.locator("[data-at]");
  await expect(at).toHaveCount(1);
  await expect(at).toHaveAttribute("data-key", "o:~Pod~kiln~kiln-cd9dc7b7-rsmwz");
  const where = isMobile ? at.locator("xpath=following-sibling::li[1]") : pane.locator(".jp-side");
  if (isMobile) {
    await expect(pane.locator(".jp-side")).toHaveCount(0);
    await expect(where).toHaveClass(/jp-detail/);
    await expect(where.locator(".jp-strip")).toBeVisible();
  } else await expect(pane.locator(".jp-detail")).toHaveCount(0);
  const out = where.locator(".ship-output");
  await expect(out.locator(".ship-output__name")).toHaveText("Pod · kiln-cd9dc7b7-rsmwz · kiln");
  await expect(out.locator(".dp-w button")).toHaveText(["Logs", "Events", "Manifest"]);
  await expect(out.locator('.dp-w button[aria-pressed="true"]')).toHaveText("Logs");
  // It follows its end: the newest line is in view.
  await expect(out.locator(".xterm-rows")).toContainText("conversation feed caught up", { timeout: 15_000 });
  // Flat: no box round the strip, the list or the detail.
  for (const sel of [".jp-strip", ".jp-list", ".ship-output"]) {
    const r = await pane
      .locator(sel)
      .first()
      .evaluate((el) => ({ radius: getComputedStyle(el).borderTopLeftRadius, left: getComputedStyle(el).borderLeftWidth }));
    expect(r.radius, sel).toBe("0px");
    expect(r.left, sel).toBe("0px");
  }
});

test("an object that differs shows its Diff; Events and Manifest read; Sync applies what's picked of what differs", async ({ page }) => {
  const pane = await open(page);
  await pickLine(pane, "o:~ConfigMap~kiln~kiln-config");
  await expect(said(pane)).toContainText("ConfigMap kiln-config picked · differs from git · Sync applies just this object");
  const out = pane.locator(".ship-output[data-key='~ConfigMap~kiln~kiln-config']");
  await expect(out.locator(".dp-w button")).toHaveText(["Diff", "Events", "Manifest"]);
  await expect(out.locator(".dp-op .rm").first()).toContainText("log_level: info");
  await expect(out.locator(".dp-op .ad").first()).toContainText("log_level: debug");
  await expect(out.locator(".dp-op")).toContainText("live (left) against git at 021a29f (right) · 2 keys differ");
  await out.locator(".dp-w button", { hasText: "Events" }).click();
  await expect(out.locator(".dp-op")).toContainText("Readiness probe failed");
  // A long event goes on under itself: nothing in the detail scrolls sideways.
  expect(await out.locator(".dp-op").evaluate((el) => el.scrollWidth - el.clientWidth)).toBeLessThanOrEqual(0);
  await out.locator(".dp-w button", { hasText: "Manifest" }).click();
  await expect(out.locator(".dp-op")).toContainText("kind: ConfigMap");
  await verb(pane, "Sync").click();
  await expect(said(pane)).toContainText("Syncing ConfigMap kiln-config of kiln-production.");
  expect(await last(page)).toEqual({ app: KILN, verb: "sync", resources: ["~ConfigMap~kiln~kiln-config"] });
  // "out of sync" picks what differs: the workload and its config.
  await strip(pane).locator(".jp-q button", { hasText: "out of sync" }).click();
  await expect(said(pane)).toContainText("2 out of sync picked");
  await verb(pane, "Sync").click();
  await expect(said(pane)).toContainText("Syncing 2 objects of kiln-production.");
  expect(await last(page)).toEqual({ app: KILN, verb: "sync", resources: ["apps~Deployment~kiln~kiln", "~ConfigMap~kiln~kiln-config"] });
  // A pod alone differs in nothing: Sync syncs what differs, all of it.
  await pickLine(pane, "o:~Pod~kiln~kiln-cd9dc7b7-rsmwz");
  await verb(pane, "Sync").click();
  await expect(said(pane)).toContainText("Syncing kiln-production to what git renders.");
  expect(await last(page)).toEqual({ app: KILN, verb: "sync" });
});

test("Refresh asks: Refresh or Hard refresh; Keep sends nothing", async ({ page }) => {
  const pane = await open(page);
  const tall = async () => (await strip(pane).boundingBox())?.height ?? 0;
  const before = await tall();
  await verb(pane, "Refresh").click();
  await expect(verbs(pane)).toHaveText(["Keep", "Refresh", "Hard refresh"]);
  await expect(said(pane)).toContainText("Hard refresh also renders deploy/helm/kiln from scratch, its cache dropped.");
  await expect(strip(pane).locator(".jp-say")).toHaveAttribute("data-tone", "ask");
  // The question has the words' room, whole, and the strip keeps its height.
  await expect(strip(pane).locator(".jp-q")).toBeHidden();
  expect(await said(pane).evaluate((el) => el.scrollHeight <= el.clientHeight + 1)).toBe(true);
  expect(await tall()).toBe(before);
  const bodies = (await sent(page)).length;
  await verb(pane, "Keep").click();
  await expect(verbs(pane)).toHaveText(["Sync", "Refresh", "Roll back", "Stop"]);
  expect((await sent(page)).length).toBe(bodies);
  await verb(pane, "Refresh").click();
  await verb(pane, "Hard refresh").click();
  await expect(said(pane)).toContainText("Rendering kiln-production again and comparing it.");
  expect(await last(page)).toEqual({ app: KILN, verb: "refresh", hard: true });
  await verb(pane, "Refresh").click();
  await strip(pane)
    .locator(".jp-verbs button", { hasText: /^Refresh$/ })
    .click();
  expect(await last(page)).toEqual({ app: KILN, verb: "refresh", hard: false });
});

test("History: any commit can be rolled back to, asked again; what runs now can't", async ({ page }) => {
  const pane = await open(page);
  // d90465f, never deployed: where it sits among the deploys, and what it would change.
  await pickLine(pane, "h:3");
  await expect(said(pane)).toContainText("d90465f picked · never deployed · Roll back takes kiln-production to it");
  const card = pane.locator(".jp-cm[data-key='h:3']");
  await expect(card).toContainText("d90465f · never deployed");
  await expect(card).toContainText("between #151 and #153");
  await expect(card).toContainText("Against what runs (#153)");
  await expect(card).toContainText("Deployment kiln: image kiln:15878aa → kiln:d90465f");
  await expect(card).toContainText("20 objects the same");
  await verb(pane, "Roll back").click();
  await expect(verbs(pane)).toHaveText(["Keep", "Roll back"]);
  await expect(said(pane)).toContainText("Roll kiln-production back to d90465f? It was never deployed");
  await verb(pane, "Roll back").click();
  await expect(said(pane)).toContainText("Rolling kiln-production back to d90465f.");
  expect(await last(page)).toEqual({ app: KILN, verb: "rollback", revision: "d90465ff49aa11bb22cc33dd44ee55ff66aa77bb" });
  // #151, an earlier deploy: by its number.
  await pickLine(pane, "h:4");
  await verb(pane, "Roll back").click();
  await expect(said(pane)).toContainText("Roll kiln-production back to #151 · b626724?");
  await verb(pane, "Roll back").click();
  expect(await last(page)).toEqual({ app: KILN, verb: "rollback", to: 151 });
  // What runs now isn't gone back to.
  await pickLine(pane, "h:2");
  await expect(verb(pane, "Roll back")).toBeDisabled();
});

test("Promote and Abort while a rollout runs; Abort asks again", async ({ page }) => {
  const pane = await open(page, "default/quiver-production");
  await expect(verbs(pane)).toHaveText(["Promote", "Refresh", "Roll back", "Abort"]);
  await expect(pane.locator(".jp-g > span:first-child").first()).toHaveText("Rollout #39");
  await expect(pane.locator("[data-at]")).toHaveAttribute("data-key", "s:2");
  await expect(said(pane)).toContainText("Rollout #39 picked · 20% · step 2 of 3 · waiting for approval · Promote moves it on now");
  const card = pane.locator(".jp-cm").first();
  await expect(card).toContainText("Step 2 · a person approves");
  // Its traffic: what ran before, and what it rolls out.
  await expect(card).toContainText("#38 · what ran before · 80%");
  await expect(card).toContainText("#39 · rolling out · 20%");
  await verb(pane, "Promote").click();
  await expect(said(pane)).toContainText("Promoting the rollout past its step.");
  expect(await last(page)).toEqual({ app: "default/quiver-production", verb: "promote", rollout: "in-cluster|apps/Rollout/quiver/quiver" });
  await verb(pane, "Abort").click();
  await expect(verbs(pane)).toHaveText(["Keep", "Abort it"]);
  await expect(said(pane)).toContainText("Abort the rollout of quiver-production?");
  await expect(strip(pane).locator(".jp-say")).toHaveAttribute("data-tone", "warn");
  await verb(pane, "Abort it").click();
  expect(await last(page)).toEqual({ app: "default/quiver-production", verb: "abort", rollout: "in-cluster|apps/Rollout/quiver/quiver" });
});

test("Stop is for a sync under way, asked again; Sync waits for it", async ({ page }) => {
  const pane = await open(page, "default/quiver-staging");
  await expect(verb(pane, "Sync")).toBeDisabled();
  await expect(verb(pane, "Stop")).toBeEnabled();
  await verb(pane, "Stop").click();
  await expect(verbs(pane)).toHaveText(["Keep", "Stop the sync"]);
  await verb(pane, "Stop the sync").click();
  await expect(said(pane)).toContainText("Stopping quiver-staging's sync.");
  expect(await last(page)).toEqual({ app: "default/quiver-staging", verb: "stop" });
});

test("a setting picked: its change in the strip, the rest dimmed, never hidden", async ({ page }) => {
  const pane = await open(page);
  await pickLine(pane, "c:auto_sync");
  await expect(said(pane)).toContainText("Auto-sync picked · off, as .kiln/deploy.yaml says");
  await expect(verbs(pane)).toHaveText(["Turn on", "Refresh", "Roll back", "Stop"]);
  for (const v of ["Refresh", "Roll back", "Stop"]) await expect(verb(pane, v)).toBeDisabled();
  await expect(pane.locator(".jp-cm").first()).toContainText("Turning it on here holds until that file changes it.");
  await verb(pane, "Turn on").click();
  await expect(said(pane)).toContainText("Auto-sync is on for kiln-production.");
  expect(await last(page)).toEqual({ app: KILN, verb: "auto_sync", on: true });
  // Now kiln syncs it by itself: the row waits for that, no one else; the setting says on.
  await expect(line(pane, "c:auto_sync")).toContainText("on");
  await expect(page.locator(`.dp-row[data-app="${KILN}"] .ship-text`)).toContainText("auto-sync syncs it");
  // Prune is its deploy file's to set.
  await pickLine(pane, "c:prune");
  await expect(verb(pane, "Turn on")).toBeDisabled();
});

test("Delete, Settings' last line: dimmed where a deploy file declares the app; else asked again, and its row goes", async ({ page }) => {
  const pane = await open(page);
  await expect(line(pane, "c:delete")).toContainText(/App\s*delete kiln-production/);
  await pickLine(pane, "c:delete");
  await expect(said(pane)).toContainText("Delete kiln-production picked · its repository's .kiln/deploy.yaml declares it: take it out there");
  await expect(verbs(pane)).toHaveText(["Delete", "Refresh", "Roll back", "Stop"]);
  for (const v of ["Delete", "Refresh", "Roll back", "Stop"]) await expect(verb(pane, v)).toBeDisabled();
  // The chart made here can go: asked again, Keep sends nothing, its objects left where they are.
  const PROM = "default/kube-prometheus-stack-production";
  const prom = await open(page, PROM);
  await pickLine(prom, "c:delete");
  await expect(said(prom)).toContainText("Delete kube-prometheus-stack-production picked · its 140 objects go too, unless you leave them");
  await expect(prom.locator(".jp-cm").first()).toContainText("Its deploy history goes with it");
  await verb(prom, "Delete").click();
  await expect(verbs(prom)).toHaveText(["Keep", "Delete all", "Delete, leave objects"]);
  await expect(said(prom)).toContainText("Delete kube-prometheus-stack-production? kiln stops deploying it.");
  await expect(strip(prom).locator(".jp-say")).toHaveAttribute("data-tone", "warn");
  const before = (await sent(page)).length;
  await verb(prom, "Keep").click();
  expect((await sent(page)).length).toBe(before);
  await verb(prom, "Delete").click();
  await verb(prom, "Delete, leave objects").click();
  await expect(page.locator(`.dp-row[data-app="${PROM}"]`)).toHaveCount(0);
  expect(await last(page)).toEqual({ app: PROM, verb: "delete", cascade: "orphan" });
  await expect(page.locator(".dp-proj > span:last-child").last()).toHaveText("2 apps · 1 out of sync");
});

test("what an action couldn't do says why in the strip", async ({ page }) => {
  const pane = await open(page, "default/site-production");
  // #19 failed its health check: rolling back to it is refused.
  await pickLine(pane, "h:1");
  await verb(pane, "Roll back").click();
  await verb(pane, "Roll back").click();
  await expect(said(pane)).toContainText("roll back to a deploy that held");
  await expect(strip(pane).locator(".jp-say")).toHaveAttribute("data-tone", "warn");
});

test("nothing moves as an app's lines are picked, acted on, and CD says it changed", async ({ page, browserName }) => {
  test.skip(browserName !== "chromium", "layout shifts are Chromium's to count");
  test.setTimeout(90_000);
  const pane = await open(page);
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
  for (const k of ["o:~ConfigMap~kiln~kiln-config", "w:apps~Deployment~kiln~kiln", "h:3", "c:auto_sync", "h:0", "o:~Pod~kiln~kiln-cd9dc7b7-rsmwz"]) {
    await pickLine(pane, k);
    await page.waitForTimeout(600);
  }
  await verb(pane, "Refresh").click();
  await page.waitForTimeout(300);
  await verb(pane, "Keep").click();
  await verb(pane, "Sync").click();
  await expect(said(pane)).toContainText("Syncing");
  // CD says what changed: the list and the pane ask again, twice.
  for (let i = 0; i < 2; i++) {
    await page.request.post("/api/mock/ship/deploys/tick");
    await page.waitForTimeout(2_000);
  }
  await expect(page.locator('.dp-row[data-app="default/quiver-staging"] .ship-text')).toContainText("wave 3 of 3");
  await page.waitForTimeout(5_000);
  const seen = await page.evaluate(() => ({ shift: (window as unknown as { __shift: number }).__shift, moved: (window as unknown as { __moved: string[] }).__moved }));
  expect(seen.shift, `what moved: ${seen.moved.join(" | ")}`).toBe(0);
});

for (const width of [320, 390, 1280]) {
  test(`at ${width}, the opened app scrolls nothing sideways`, async ({ page, isMobile }) => {
    test.skip(isMobile, "each width is set here; the desktop projects run them");
    await page.setViewportSize({ width, height: 900 });
    const pane = await open(page);
    await pickLine(pane, "h:3");
    await verb(pane, "Roll back").click();
    await expect(verb(pane, "Keep")).toBeVisible();
    expect(await page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth)).toBe(0);
    for (const sel of [".jp-strip", ".jp-verbs button", ".jp-list", ".jp-j", ".dp-proj"]) {
      const right = await page.locator(sel).evaluateAll((els) => Math.max(...els.map((n) => n.getBoundingClientRect().right)));
      expect(right, sel).toBeLessThanOrEqual(width + 0.5);
    }
    // A pod's events, their long lines wrapped: nothing in the detail scrolls sideways either.
    await verb(pane, "Keep").click();
    await pickLine(pane, "o:~Pod~kiln~kiln-cd9dc7b7-rsmwz");
    const out = pane.locator(".ship-output").first();
    await out.locator(".dp-w button", { hasText: "Events" }).click();
    const op = out.locator(".dp-op:not([aria-busy])");
    await expect(op).toContainText("Readiness probe failed");
    expect(await op.evaluate((el) => el.scrollWidth - el.clientWidth)).toBeLessThanOrEqual(0);
    expect(await page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth)).toBe(0);
  });
}
