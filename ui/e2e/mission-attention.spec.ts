/** Mission Control attention plan 01a199e1b177752e84dad5dc990e6a15, desktop/phone Chromium/WebKit. */
import { expect, test } from "@playwright/test";
import type { AttentionRequest } from "../src/api/attention";
test.skip(!!process.env.E2E_BASE_URL, "The scenarios organize only simulated agents");
test.beforeEach(async ({ request }) => {
  if (process.env.E2E_BASE_URL) return;
  const reset = await request.post("/api/mock/attention/reset");
  expect(reset.ok()).toBe(true);
});

test("mc_attention: Updated is independent of zero-work Finished and clears in place", async ({ page }) => {
  let ownWork = 0;
  await page.route(/\/api\/harness\/attention(?:\?.*)?$/, async (route) => {
    const response = await route.fetch();
    if (route.request().method() !== "GET") {
      await route.fulfill({ response });
      return;
    }
    const snapshot = (await response.json()) as { agents: Record<string, unknown>; request_order?: string[] };
    snapshot.agents["fam-migr"] = {
      revision: 0,
      urgency: { known: true, score: 4, stale: true, since: 1, waited: 4, expected_wait: 1, observations: 1, reason: "Waiting longer than previous answered requests" },
    };
    snapshot.request_order = ["fam-migr"];
    await route.fulfill({ response, json: snapshot });
  });
  await page.route("**/api/harness/sessions", async (route) => {
    const response = await route.fetch();
    const agents = (await response.json()) as { id: string; [key: string]: unknown }[];
    await route.fulfill({
      response,
      json: agents.map((a) =>
        a.id === "fam-docs"
          ? { ...a, status: "idle", tasks_running: ownWork, left_behind: 0, subagents: [], last_turn: { at: Date.now() - 1000, how: "done" } }
          : a,
      ),
    });
  });
  await page.goto("/agents");
  const row = page.locator('.ms-row[data-agent="fam-docs"]');
  await expect(row.locator(".ms-word")).toHaveText("Finished");
  await expect(page.locator('.ms-row[data-agent="fam-migr"] .ms-stale')).toHaveText("Stale");
  await row.getByRole("button", { name: "Clear Updated for Update the docs", exact: true }).click();
  await expect(row.locator(".ms-updated")).toHaveCount(0);
  await expect(row.locator(".ms-word")).toHaveText("Finished");
  await expect(page).toHaveURL(/\/agents$/);
  await expect(page.locator('.ms-row[data-agent="fam-tests"]')).toBeVisible();
  await expect(page.locator('.ms-row[data-agent="fam-orch"]')).toBeVisible();
  ownWork = 1;
  await page.reload();
  await expect(row.locator(".ms-word")).toHaveText("Ready");
  await row.getByRole("button", { name: "Open Update the docs", exact: true }).click();
  await expect(page.locator(".ag-turn-control")).toHaveAttribute("data-state", "ready");
});

test("mc_attention: park and undo restore the row, archive and restore keep the chat", async ({ page }, testInfo) => {
  const requestAt = Date.now() - 60_000;
  let requestWhy = "Review the test result";
  let deferred: AttentionRequest[] | undefined;
  await page.route(/\/api\/harness\/attention(?:\?.*)?$/, async (route) => {
    if (route.request().method() === "POST") {
      const command = route.request().postDataJSON() as { action?: string; agent?: string };
      if (command.action === "park" && command.agent === "fam-tests") deferred = [{ agent: "fam-tests", kind: "held", id: requestWhy, at: requestAt }];
    }
    const response = await route.fetch();
    const snapshot = (await response.json()) as { agents: Record<string, { deferred_requests?: AttentionRequest[] }> };
    if (deferred && snapshot.agents?.["fam-tests"]) snapshot.agents["fam-tests"].deferred_requests = deferred;
    await route.fulfill({ response, json: snapshot });
  });
  await page.route("**/api/harness/sessions", async (route) => {
    const response = await route.fetch();
    const agents = (await response.json()) as { id: string; [key: string]: unknown }[];
    await route.fulfill({ response, json: agents.map((a) => (a.id === "fam-tests" ? { ...a, status: "idle", held: { why: requestWhy, since: requestAt } } : a)) });
  });
  await page.goto("/agents");
  const pending = page.locator('.ms-row[data-agent="fam-tests"]');
  await pending.getByRole("button", { name: "Organize Fix the flaky retry test", exact: true }).click();
  await pending.getByRole("button", { name: "Park", exact: true }).click();
  await expect(page.locator('.ms-list > .ms-row[data-agent="fam-tests"]')).toHaveCount(0);
  requestWhy = "A different decision at the same runner time";
  await page.reload();
  await expect(page.locator('.ms-list > .ms-row[data-agent="fam-tests"]')).toBeVisible();
  const row = page.locator('.ms-row[data-agent="fam-docs"]');
  await row.getByRole("button", { name: "Organize Update the docs", exact: true }).click();
  await row.getByRole("button", { name: "Pin", exact: true }).click();
  await expect(row.getByRole("img", { name: "Pinned" })).toBeVisible();
  const lead = page.locator('.ms-row[data-agent="fam-orch"]');
  await lead.getByRole("button", { name: "Organize Release prep", exact: true }).click();
  await lead.getByRole("button", { name: "Pin", exact: true }).click();
  await lead.getByRole("button", { name: "Move pin up", exact: true }).click();
  await page.reload();
  await expect
    .poll(async () => {
      const ids = await page.locator(".ms-list > .ms-row").evaluateAll((rows) => rows.map((r) => r.getAttribute("data-agent")));
      return ids.indexOf("fam-orch") >= 0 && ids.indexOf("fam-orch") < ids.indexOf("fam-docs");
    })
    .toBe(true);
  await row.getByRole("button", { name: "Organize Update the docs", exact: true }).click();
  await row.getByRole("button", { name: "Park", exact: true }).click();
  await expect(page.locator('.ms-list > .ms-row[data-agent="fam-docs"]')).toHaveCount(0);
  await page.locator(".ms-attention-notice").getByRole("button", { name: "Undo", exact: true }).click();
  await expect(row).toBeVisible();
  await row.getByRole("button", { name: "Organize Update the docs", exact: true }).click();
  await row.getByRole("button", { name: "Archive…", exact: true }).click();
  await row.getByRole("button", { name: "Archive", exact: true }).click();
  await expect(row).toHaveCount(0);
  await page.getByLabel("Show chats", { exact: true }).selectOption("archived");
  await expect(row).toBeVisible();
  await row.getByRole("button", { name: "Organize Update the docs", exact: true }).click();
  await row.getByRole("button", { name: "Restore", exact: true }).click();
  await page.getByLabel("Show chats", { exact: true }).selectOption("active");
  await expect(row).toBeVisible();
  const overflow = await page.evaluate(() => document.documentElement.scrollWidth > window.innerWidth);
  expect(overflow).toBe(false);
  await page.screenshot({ path: testInfo.outputPath("mission-attention.png"), fullPage: true });
  await page.screenshot({ path: testInfo.outputPath("mission-attention-viewport.jpg"), quality: 30, fullPage: false });
});

test("mc_attention: settings expose opt-out and automation is opt-in", async ({ page }) => {
  await page.goto("/agents");
  await page.getByRole("button", { name: "Attention settings", exact: true }).click();
  await expect(page.getByRole("checkbox", { name: "Automatically park quiet chats" })).not.toBeChecked();
  await expect(page.getByRole("checkbox", { name: "Automatically archive parked chats" })).not.toBeChecked();
  await page.getByLabel("Later park delay in minutes", { exact: true }).fill("90");
  await page.getByLabel("Evening park hour", { exact: true }).fill("20");
  await page.getByLabel("Morning park hour", { exact: true }).fill("7");
  await page.getByRole("button", { name: "Save", exact: true }).click();
  await expect(page.locator(".ms-attention-notice")).toContainText("Attention settings saved");
  await page.reload();
  await page.getByRole("button", { name: "Attention settings", exact: true }).click();
  await expect(page.getByLabel("Later park delay in minutes", { exact: true })).toHaveValue("90");
  await expect(page.getByLabel("Evening park hour", { exact: true })).toHaveValue("20");
  await expect(page.getByLabel("Morning park hour", { exact: true })).toHaveValue("7");
  await page.getByRole("button", { name: "Cancel", exact: true }).click();
  await page.getByLabel("Sort agents", { exact: true }).selectOption("yours");
  await expect(page.getByLabel("Sort agents", { exact: true })).toHaveValue("yours");
});

test("mc_attention: SPA return samples a new order and records the exposure that was displayed", async ({ page }) => {
  let reads = 0;
  let order = ["fam-docs", "fam-orch"];
  let exposure = "first-visit";
  const opened: { agent?: string; exposure?: string }[] = [];
  await page.route(/\/api\/harness\/attention(?:\?.*)?$/, async (route) => {
    if (route.request().method() !== "GET") {
      const command = route.request().postDataJSON() as { action?: string; agent?: string; exposure?: string };
      if (command.action === "visit") opened.push(command);
      await route.continue();
      return;
    }
    reads++;
    const response = await route.fetch();
    const snapshot = (await response.json()) as Record<string, unknown>;
    await route.fulfill({ response, json: { ...snapshot, order, exposure } });
  });
  const ids = () => page.locator(".ms-list > .ms-row").evaluateAll((rows) => rows.map((r) => r.getAttribute("data-agent")));
  await page.goto("/agents");
  await expect.poll(ids).toEqual(expect.arrayContaining(order));
  const firstRows = await ids();
  expect(firstRows.indexOf("fam-docs")).toBeLessThan(firstRows.indexOf("fam-orch"));
  await page.getByRole("button", { name: "Open Update the docs", exact: true }).click();
  await expect(page).toHaveURL(/\/agents\/fam-docs$/);
  await expect.poll(() => opened.some((c) => c.agent === "fam-docs" && c.exposure === "first-visit")).toBe(true);
  const previousReads = reads;
  order = ["fam-orch", "fam-docs"];
  exposure = "return-visit";
  await page.goBack();
  await expect(page).toHaveURL(/\/agents$/);
  await expect.poll(() => reads).toBeGreaterThan(previousReads);
  await expect
    .poll(async () => {
      const rows = await ids();
      return rows.indexOf("fam-orch") >= 0 && rows.indexOf("fam-orch") < rows.indexOf("fam-docs");
    })
    .toBe(true);
  await page.getByRole("button", { name: "Open Release prep", exact: true }).click();
  await expect.poll(() => opened.some((c) => c.agent === "fam-orch" && c.exposure === "return-visit")).toBe(true);
});

test("mc_attention: an expired native conversation retains its archive until saved-copy consent", async ({ page }) => {
  let copies = 0;
  await page.route(/\/api\/harness\/attention(?:\?.*)?$/, async (route) => {
    if (route.request().method() === "POST") {
      const action = route.request().postDataJSON() as { action?: string };
      if (action.action === "restore") {
        await route.fulfill({
          status: 409,
          contentType: "application/json",
          body: JSON.stringify({ error: "The original harness conversation is no longer available. Continue from kiln's saved conversation." }),
        });
        return;
      }
      if (action.action === "restore_copy") copies++;
    }
    await route.continue();
  });
  await page.goto("/agents");
  const row = page.locator('.ms-row[data-agent="fam-docs"]');
  await row.getByRole("button", { name: "Organize Update the docs", exact: true }).click();
  await row.getByRole("button", { name: "Archive…", exact: true }).click();
  await row.getByRole("button", { name: "Archive", exact: true }).click();
  await page.getByLabel("Show chats", { exact: true }).selectOption("archived");
  await row.getByRole("button", { name: "Organize Update the docs", exact: true }).click();
  await row.getByRole("button", { name: "Restore", exact: true }).click();
  await expect(page.locator(".ms-attention-error")).toContainText("no longer available");
  await expect(row).toBeVisible();
  await row.getByRole("button", { name: "Continue from saved conversation…", exact: true }).click();
  await expect(row.locator(".ms-attention-confirm")).toContainText("new harness session");
  expect(copies).toBe(0);
  await row.getByRole("button", { name: "Continue from saved conversation", exact: true }).click();
  await expect.poll(() => copies).toBe(1);
  await expect(row).toHaveCount(0);
  await page.getByLabel("Show chats", { exact: true }).selectOption("active");
  await expect(row).toBeVisible();
});

test("mc_attention: rendered pairs use actual positions, freeze on refresh and redraw after your answer", async ({ page }, testInfo) => {
  type Frame = { action: string; exposure: string; frame_revision: number; context: string; order_a: string[]; order_b: string[]; shown: string[]; model_order: boolean };
  const frames: Frame[] = [];
  const closes: { exposure: string; frame_revision: number }[] = [];
  const opened: { agent: string; exposure: string; chosen_position: number; frame_revision: number }[] = [];
  let arrivals = 0;
  let refreshes = 0;
  const assigned = new Map<string, { key: string; order_a: string[]; order_b: string[]; arm: number; model_order: boolean }>();
  await page.route(/\/api\/harness\/attention(?:\?.*)?$/, async (route) => {
    const req = route.request();
    if (req.method() === "POST") {
      const body = req.postDataJSON() as Frame & { agent: string; chosen_position: number };
      if (body.action === "render") frames.push(body);
      if (body.action === "close") closes.push(body);
      if (body.action === "visit") opened.push(body);
      await route.continue();
      return;
    }
    const response = await route.fetch();
    const snapshot = (await response.json()) as { order: string[]; [key: string]: unknown };
    const url = new URL(req.url());
    const visit = url.searchParams.get("visit") ?? "unknown";
    if (url.searchParams.get("mode") === "arrival") {
      if (!assigned.has(visit)) {
        arrivals++;
        const rest = snapshot.order.filter((id) => id !== "fam-docs" && id !== "fam-orch");
        const first = arrivals % 2 ? ["fam-docs", "fam-orch"] : ["fam-orch", "fam-docs"];
        assigned.set(visit, { key: `rendered-${arrivals}`, order_a: [...first, ...rest], order_b: [...first].reverse().concat(rest), arm: 0, model_order: true });
      }
    } else refreshes++;
    // Deliberately offer a different pair on a refresh. The client must retain its displayed pair.
    const pair =
      url.searchParams.get("mode") === "refresh" ? { key: "unshown-refresh", order_a: [...snapshot.order].reverse(), order_b: snapshot.order, arm: 0, model_order: true } : assigned.get(visit);
    await route.fulfill({ response, json: { ...snapshot, order: pair?.order_a ?? [], pair, exposure: pair?.key } });
  });
  await page.route("**/api/harness/sessions/fam-migr/command", (route) => route.fulfill({ json: {} }));
  const ids = () => page.locator(".ms-row[data-agent]").evaluateAll((rows) => rows.filter((r) => r.getClientRects().length > 0).map((r) => r.getAttribute("data-agent")));
  await page.goto("/agents");
  await expect.poll(() => frames).toEqual(expect.arrayContaining([expect.objectContaining({ exposure: "rendered-1", model_order: true })]));
  const actual = await ids();
  const firstFrame = frames.findLast((f) => f.exposure === "rendered-1");
  expect(firstFrame?.shown).toEqual(actual);
  expect(firstFrame?.order_a).toEqual(actual);
  expect(firstFrame?.context).toBe(testInfo.project.name.startsWith("phone") ? "mission/phone" : "mission/desktop");
  const beforeRefresh = refreshes;
  await page.evaluate(() => window.dispatchEvent(new Event("focus")));
  await expect.poll(() => refreshes).toBeGreaterThan(beforeRefresh);
  expect(await ids()).toEqual(actual);
  expect(frames.some((f) => f.exposure === "unshown-refresh")).toBe(false);
  await page.locator('.ms-row[data-agent="fam-migr"]').getByRole("button", { name: "Allow", exact: true }).click();
  await expect.poll(() => arrivals).toBe(2);
  await expect.poll(() => frames.some((f) => f.exposure === "rendered-2")).toBe(true);
  await expect
    .poll(async () => {
      const rows = await ids();
      return rows.indexOf("fam-orch") < rows.indexOf("fam-docs");
    })
    .toBe(true);
  expect(closes.some((c) => c.exposure === "rendered-1")).toBe(true);
  // A client-only filter after seeing a draw is conservatively excluded until the next arrival.
  await page.getByPlaceholder("Filter by name, runner, harness, state…").fill("Update the docs");
  await expect.poll(() => frames.findLast((f) => f.exposure === "rendered-2")?.model_order).toBe(false);
  const filtered = await ids();
  await page.getByRole("button", { name: "Open Update the docs", exact: true }).click();
  await expect.poll(() => opened.some((c) => c.exposure === "rendered-2" && c.agent === "fam-docs" && c.chosen_position === filtered.indexOf("fam-docs"))).toBe(true);
});

test("mc_attention: leaving the list without a row choice closes its rendered visit", async ({ page, context }) => {
  const renders: { exposure: string; frame_revision: number }[] = [];
  const closes: { exposure: string; frame_revision: number }[] = [];
  await context.route(/\/api\/harness\/attention(?:\?.*)?$/, async (route) => {
    if (route.request().method() === "POST") {
      const body = route.request().postDataJSON() as { action: string; exposure: string; frame_revision: number };
      if (body.action === "render") renders.push(body);
      if (body.action === "close") closes.push(body);
    }
    await route.continue();
  });
  await page.goto("/agents");
  await expect.poll(() => renders.length).toBeGreaterThan(0);
  const rendered = renders.at(-1);
  await page.getByRole("link", { name: "kiln overview", exact: true }).click();
  await expect.poll(() => closes.some((c) => c.exposure === rendered?.exposure && c.frame_revision === rendered.frame_revision)).toBe(true);
  await page.goto("/agents");
  await expect.poll(() => renders.some((frame) => frame.exposure !== rendered?.exposure)).toBe(true);
  const beforeUnload = renders.at(-1);
  await page.goto("/agents/programs");
  // Chrome's unload request can outlive routing interception; verify both exact records at the server.
  await expect
    .poll(async () => {
      const response = await context.request.get("/api/mock/attention/visits");
      const { visits } = (await response.json()) as { visits: { exposure: string; rendered_revision: number | null; closed_revision: number | null }[] };
      return visits.some((visit) => visit.exposure === beforeUnload?.exposure && visit.rendered_revision === beforeUnload.frame_revision && visit.closed_revision === beforeUnload.frame_revision);
    })
    .toBe(true);
});
