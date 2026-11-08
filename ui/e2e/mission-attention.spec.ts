/**
 * Mission control kept around your attention (plan 01a199e1b177752e84dad5dc990e6a15; docs/agents.md,
 * "Activity and attention"), in the production build against the mock, on a desktop and a phone:
 * Updated, the row's ⋯ menu (pin, park, archive), the Undo where a row was, the Pinned section, a stale
 * request's Park and Archive, Archived under the State filter with Restore and the saved copy, a
 * swipe to park by touch, your own actions moving only the rows they acted on, what was shown going
 * with what you do, and the settings in Settings → You → Attention. Each scenario is set by the mock's
 * own controls and what the page asks is only watched, never answered in its place.
 */
import { expect, test, type Page, type Request } from "@playwright/test";

test.skip(!!process.env.E2E_BASE_URL, "The scenarios organize only simulated agents");
test.beforeEach(async ({ request }) => {
  expect((await request.post("/api/mock/attention/reset")).ok()).toBe(true);
});

const row = (page: Page, id: string) => page.locator(`li.ms-row[data-agent="${id}"]`);
/** The live list's rows, in order. */
const ids = (page: Page) => page.locator('ul[data-list="live"] > li.ms-row').evaluateAll((rows) => rows.map((r) => r.getAttribute("data-agent") ?? ""));
async function open(page: Page) {
  await page.goto("/agents");
  await expect(page.locator('ul[data-list="live"] > li.ms-row').first()).toBeVisible();
}
/** The row's ⋯ menu, and one of its items. */
async function menu(page: Page, id: string, item: string) {
  await row(page, id)
    .getByRole("button", { name: /^Actions for / })
    .click();
  await page.getByRole("menuitem", { name: item, exact: true }).click();
}
/** What the page sent to /api/harness/attention, as it sent it. */
function watch(page: Page) {
  const sent: { method: string; mode: string | null; visit: string | null; body: Record<string, unknown> | null }[] = [];
  page.on("request", (r: Request) => {
    const url = new URL(r.url());
    if (url.pathname !== "/api/harness/attention") return;
    const body = r.method() === "POST" ? (r.postDataJSON() as Record<string, unknown>) : null;
    sent.push({ method: r.method(), mode: (body?.mode as string | undefined) ?? url.searchParams.get("mode"), visit: (body?.visit as string | undefined) ?? url.searchParams.get("visit"), body });
  });
  return sent;
}
/** An agent of the mock as a scenario needs it, and as it was, to put it back. */
async function patch(page: Page, id: string, to: Record<string, unknown>) {
  const agents = (await (await page.request.get("/api/harness/sessions")).json()) as Record<string, unknown>[];
  const was = agents.find((a) => a.id === id) ?? {};
  await page.request.post("/api/mock/agent-patch", { data: { id, patch: to } });
  return () => page.request.post("/api/mock/agent-patch", { data: { id, patch: Object.fromEntries(Object.keys(to).map((k) => [k, was[k] ?? null])) } });
}

test("mc_attention: Updated shows for a chat that finished while you were elsewhere, and clears from its row without opening it", async ({ page }) => {
  await open(page);
  // Elsewhere: another page, while "Update the docs" finishes its turn.
  await page.getByRole("link", { name: "kiln overview", exact: true }).click();
  await expect(page).not.toHaveURL(/\/agents$/);
  const back = await patch(page, "fam-docs", { status: "idle", tasks_running: 0, left_behind: 0, last_turn: { at: Date.now(), how: "done" } });
  try {
    await page.goBack();
    const docs = row(page, "fam-docs");
    await expect(docs.locator('[data-flag="updated"]')).toHaveText("Updated");
    await expect(docs.locator(".ms-word")).toHaveText("Finished");
    const order = await ids(page);
    await menu(page, "fam-docs", "Clear Updated");
    await expect(docs.locator('[data-flag="updated"]')).toHaveCount(0);
    await expect(docs.locator(".ms-word")).toHaveText("Finished");
    await expect(page).toHaveURL(/\/agents$/);
    expect(await ids(page), "clearing Updated moves no row").toEqual(order);
    // Work of its own still running: Ready, Updated or not.
    await page.request.post("/api/mock/agent-patch", { data: { id: "fam-docs", patch: { tasks_running: 1 } } });
    await expect(docs.locator(".ms-word")).toHaveText("Ready");
  } finally {
    await back();
  }
});

test("mc_attention: a park from the row's menu leaves an Undo where the row was, nothing else moving, and Undo puts it back", async ({ page }) => {
  await open(page);
  const docs = row(page, "fam-docs");
  await docs.scrollIntoViewIfNeeded();
  await menu(page, "fam-docs", "Park until…");
  const ask = docs.getByRole("group", { name: /^Park “/ });
  await expect(ask.getByRole("button", { name: "Tomorrow morning" })).toBeVisible();
  const before = await ids(page);
  const at = before.indexOf("fam-docs");
  const after = before[at + 1];
  const y = after ? await row(page, after).evaluate((el) => el.getBoundingClientRect().top + scrollY) : 0;
  await ask.getByRole("button", { name: "Tomorrow morning" }).click();
  const undo = page.locator("li.ms-undo");
  await expect(undo).toContainText("“Update the docs” parked until tomorrow at");
  await expect(row(page, "fam-docs")).toHaveCount(0);
  // The Undo stands in the row's place: the row after it hasn't moved.
  const entries = await page.locator('ul[data-list="live"] > li').evaluateAll((els) => els.map((e) => e.getAttribute("data-entry")));
  expect(entries[entries.indexOf("undo") + 1]).toBe(after ? `row:${after}` : undefined);
  if (after) expect(Math.abs((await row(page, after).evaluate((el) => el.getBoundingClientRect().top + scrollY)) - y)).toBeLessThan(80);
  await expect(page.getByRole("button", { name: /^Parked · 1/ })).toBeVisible();
  await undo.getByRole("button", { name: "Undo" }).click();
  await expect(row(page, "fam-docs")).toBeVisible();
  await expect(undo).toHaveCount(0);
  expect(await ids(page), "Undo puts it back where it was").toEqual(before);
  // Nothing at the top of the page says it: no status line, nothing pushed down.
  await expect(page.getByText("Chat organization saved")).toHaveCount(0);
});

test("mc_attention: pins are a section of their own under Needs you, in your order, kept across visits", async ({ page }) => {
  await open(page);
  for (const id of ["fam-tests", "fam-docs"]) {
    await row(page, id).scrollIntoViewIfNeeded();
    await menu(page, id, "Pin");
    await expect(row(page, id).getByRole("img", { name: "Pinned" })).toBeVisible();
  }
  const section = async () => {
    const entries = await page.locator('ul[data-list="live"] > li').evaluateAll((els) => els.map((e) => e.getAttribute("data-entry") ?? ""));
    const start = entries.indexOf("sep:pinned");
    return entries.slice(start + 1, entries.indexOf("sep:rest", start)).filter((e) => e.startsWith("row:"));
  };
  await expect.poll(section).toEqual(["row:fam-tests", "row:fam-docs"]);
  await expect(page.locator('[data-entry="sep:pinned"]')).toContainText("Pinned · 2");
  // Under Needs you: every row before the section needs you.
  const needs = await page.locator('ul[data-list="live"] > li').evaluateAll((els) => {
    const at = els.findIndex((e) => e.getAttribute("data-entry") === "sep:pinned");
    return els.slice(0, at).every((e) => e.getAttribute("data-stand") === "needs");
  });
  expect(needs).toBe(true);
  await menu(page, "fam-docs", "Move pin up");
  await expect.poll(section).toEqual(["row:fam-docs", "row:fam-tests"]);
  await page.reload();
  await expect.poll(section).toEqual(["row:fam-docs", "row:fam-tests"]);
});

test("mc_attention: a stale request offers Park and Archive beside its answers", async ({ page }) => {
  await page.request.post("/api/mock/attention/urgency", { data: { agent: "fam-migr", stale: true } });
  await open(page);
  const migr = row(page, "fam-migr");
  await expect(migr.locator('[data-flag="stale"]')).toHaveText("Stale");
  await expect(migr.getByRole("button", { name: "Allow", exact: true })).toBeVisible();
  await migr.getByRole("button", { name: "Park", exact: true }).click();
  await expect(migr.getByRole("group", { name: /^Park “/ })).toBeVisible();
  await migr.getByRole("button", { name: "Archive…", exact: true }).click();
  await expect(migr.getByRole("group", { name: /^Archive “/ })).toContainText("stops for everyone");
  // Nothing overlaps: its answers, Park and Archive each have their own room.
  const boxes = await migr.locator(".ms-decide > *").evaluateAll((els) => els.map((e) => e.getBoundingClientRect()).map((b) => [b.left, b.right, b.top, b.bottom]));
  for (let i = 1; i < boxes.length; i++) {
    const [l, , t, b] = boxes[i] as number[];
    const [, pr, pt, pb] = boxes[i - 1] as number[];
    expect(l >= pr - 0.5 || t >= pb - 0.5 || b <= pt + 0.5, "a stale row's controls overlap").toBe(true);
  }
});

test("mc_attention: archive asks first; the chat is under State: Archived, where Restore offers kiln's saved copy when it can't", async ({ page }) => {
  await open(page);
  const docs = row(page, "fam-docs");
  await docs.scrollIntoViewIfNeeded();
  await menu(page, "fam-docs", "Archive…");
  await docs
    .getByRole("group", { name: /^Archive “/ })
    .getByRole("button", { name: "Archive", exact: true })
    .click();
  await expect(page.locator("li.ms-undo")).toContainText("“Update the docs” archived");
  await expect(docs).toHaveCount(0);
  await expect(page.getByLabel("Show chats")).toHaveCount(0);
  await page.getByRole("combobox", { name: "Filter agents" }).click();
  await page.getByRole("option", { name: /^Archived/ }).click();
  await page.keyboard.press("Escape");
  await expect(docs).toBeVisible();
  await expect(docs.locator(".ms-word")).toHaveText("Archived");
  // Its harness's own conversation gone: Restore says so, and offers the saved copy, asked first.
  let copies = 0;
  await page.route(/\/api\/harness\/attention$/, async (route) => {
    const action = (route.request().postDataJSON() as { action?: string } | null)?.action;
    if (action === "restore")
      return route.fulfill({
        status: 409,
        json: {
          error: "Restore of fam-docs is pending: the original harness conversation is no longer available. Its complete group is kept; retry Restore or choose Continue from saved conversation.",
        },
      });
    if (action === "restore_copy") copies++;
    return route.continue();
  });
  await docs.getByRole("button", { name: "Restore", exact: true }).click();
  const copy = docs.getByRole("group", { name: /^Restore “/ });
  await expect(copy).toContainText("no longer available");
  await expect(docs).toBeVisible();
  await copy.getByRole("button", { name: /^Continue from kiln/ }).click();
  expect(copies).toBe(0);
  await copy.getByRole("button", { name: "Continue from the saved copy" }).click();
  await expect.poll(() => copies).toBe(1);
  await expect(docs).toHaveCount(0);
  await page.unroute(/\/api\/harness\/attention$/);
  await page.getByRole("button", { name: "Remove State: Archived" }).click();
  await expect(docs).toBeVisible();
});

test("mc_attention: by touch a swipe left offers Park and parks without blanking a row; a swipe right still selects", async ({ page, isMobile }) => {
  test.skip(!isMobile, "a phone's touches");
  await open(page);
  const swipe = (id: string, dx: number) =>
    page.locator(`li.ms-row[data-agent="${id}"] .ms-main`).evaluate(async (el, dx) => {
      el.scrollIntoView({ block: "center" });
      const box = el.getBoundingClientRect();
      const x = box.left + box.width / 2;
      const y = box.top + 20;
      const fire = (type: string, cx: number) =>
        (type === "pointerdown" ? el : (document.elementFromPoint(cx, y) ?? el)).dispatchEvent(
          new PointerEvent(type, { bubbles: true, cancelable: true, pointerId: 11, pointerType: "touch", isPrimary: true, clientX: cx, clientY: y }),
        );
      fire("pointerdown", x);
      for (let i = 1; i <= 10; i++) {
        fire("pointermove", x + (dx * i) / 10);
        await new Promise((r) => setTimeout(r, 16));
      }
      fire("pointerup", x + dx);
    }, dx);
  const blank = () =>
    page.locator("li.ms-row").evaluateAll((rows) => rows.filter((r) => getComputedStyle(r).opacity !== "1" || (r as HTMLElement).style.transform !== "").map((r) => r.getAttribute("data-agent")));
  await swipe("fam-docs", -120);
  const ask = row(page, "fam-docs").getByRole("group", { name: /^Park “/ });
  await expect(ask).toBeVisible();
  await expect.poll(blank).toEqual([]);
  await expect(page.locator("li.ms-row[data-selected]")).toHaveCount(0);
  await ask.getByRole("button", { name: "Tomorrow morning" }).click();
  await expect(page.locator("li.ms-undo")).toContainText("parked until tomorrow");
  await expect.poll(blank).toEqual([]);
  await swipe("fam-tests", 120);
  await expect(row(page, "fam-tests")).toHaveAttribute("data-selected", "true");
  await expect.poll(blank).toEqual([]);
});

test("mc_attention: choosing a facet saves no setting; the sort's opt-out, Needs you then yours, is kept for you", async ({ page }) => {
  const sent = watch(page);
  await open(page);
  const settings = () => sent.filter((s) => s.body?.action === "settings");
  await page.getByRole("combobox", { name: "Filter agents" }).click();
  await page.getByRole("option", { name: /^Working/ }).click();
  await page.keyboard.press("Escape");
  await expect(page.getByRole("button", { name: "Remove State: Working" })).toBeVisible();
  expect(settings(), "a facet is how the list is shown here, not a setting").toEqual([]);
  await expect(page.getByText("Attention settings saved")).toHaveCount(0);
  const sort = page.getByLabel("Sort agents", { exact: true });
  await expect(sort.locator("option:checked")).toHaveText("Needs you first");
  await sort.selectOption("yours");
  await expect.poll(() => settings().map((s) => s.body?.settings)).toEqual([{ sort: "yours" }]);
  await page.reload();
  await expect(page.getByLabel("Sort agents", { exact: true })).toHaveValue("yours");
  // Both read whole beside the filter, on a phone too.
  const cut = await sort.evaluate((el: HTMLSelectElement) => {
    const probe = document.createElement("span");
    probe.style.cssText = `position:absolute;visibility:hidden;white-space:nowrap;font:${getComputedStyle(el).font}`;
    probe.textContent = el.selectedOptions[0]?.textContent ?? "";
    document.body.append(probe);
    const wide = probe.getBoundingClientRect().width;
    probe.remove();
    const style = getComputedStyle(el);
    return wide > el.clientWidth - parseFloat(style.paddingLeft) - parseFloat(style.paddingRight);
  });
  expect(cut, "the sort's words are cut").toBe(false);
});

test("mc_attention: settings are in Settings → You → Attention, the server's own, in words", async ({ page }) => {
  await open(page);
  await expect(page.getByRole("button", { name: "Attention settings" })).toHaveCount(0);
  await page.getByRole("link", { name: "Attention settings", exact: true }).click();
  await expect(page).toHaveURL(/\/settings#you\.attention$/);
  const group = page.locator("#you\\.attention");
  await expect(group).toBeVisible();
  const park = group.getByRole("checkbox", { name: "Park quiet chats by themselves" });
  await expect(park).not.toBeChecked();
  await expect(group.getByRole("checkbox", { name: "Archive parked chats" })).toBeChecked();
  await expect(group.getByLabel("Days parked before it's archived")).toHaveValue("7");
  await park.check();
  await expect(group.getByLabel("Days quiet before it parks")).toHaveValue("3");
  await group.getByLabel("Later today").fill("90");
  await group.getByLabel("This evening").selectOption("20");
  await group.getByLabel("Tomorrow morning").selectOption("7");
  await group.getByRole("button", { name: "Save", exact: true }).click();
  await expect(group.getByRole("status")).toHaveText("Saved");
  await page.reload();
  await expect(group.getByRole("checkbox", { name: "Park quiet chats by themselves" })).toBeChecked();
  await expect(group.getByLabel("Days quiet before it parks")).toHaveValue("3");
  await expect(group.getByLabel("Later today")).toHaveValue("90");
  await expect(group.getByLabel("This evening")).toHaveValue("20");
  await expect(group.getByLabel("Tomorrow morning")).toHaveValue("7");
  // The learned order in words: no name of the server's own shows.
  await expect(group.getByRole("rowheader", { name: "Learned order", exact: true })).toBeVisible();
  await expect(group.getByRole("rowheader", { name: "Your most recent use" })).toBeVisible();
  await expect(group.getByRole("rowheader", { name: "Weight of your messages" })).toBeVisible();
  const words = (await group.innerText()).toLowerCase();
  for (const raw of ["act_r", "your_recency", "without_agent_turns", "latest_activity", "human_recency", "range_weight", "weight.", "scale.", "subagent_turn"]) expect(words).not.toContain(raw);
});

test("mc_attention: what was shown goes with a pick, and a visit left without one closes with the next snapshot request", async ({ page, context }) => {
  const sent = watch(page);
  await open(page);
  // Rendering sends nothing: what shows goes with what you do.
  await page.waitForTimeout(1000);
  expect(sent.filter((s) => s.body?.action === "render" || s.body?.action === "close")).toEqual([]);
  const shown = await ids(page);
  await page.getByRole("button", { name: "Open Update the docs", exact: true }).click();
  await expect(page).toHaveURL(/\/agents\/fam-docs$/);
  const visit = sent.find((s) => s.body?.action === "visit");
  const frame = sent.find((s) => s.body?.action === "render");
  expect(visit?.body).toMatchObject({ agent: "fam-docs", source: "list", chosen_position: shown.indexOf("fam-docs") });
  expect(frame?.body).toMatchObject({ exposure: visit?.body?.exposure, frame_revision: visit?.body?.frame_revision });
  expect((frame?.body?.shown as string[] | undefined)?.slice(0, shown.length)).toEqual(shown);
  // Back on the list, then away without a pick: nothing is sent as you leave...
  await page.goBack();
  await expect(page.locator('ul[data-list="live"] > li.ms-row').first()).toBeVisible();
  await expect.poll(() => sent.filter((s) => s.method === "GET" && s.mode === "arrival").length).toBe(2);
  const second = sent.filter((s) => s.method === "GET" && s.mode === "arrival")[1]?.visit;
  await page.getByRole("link", { name: "kiln overview", exact: true }).click();
  await expect(page).not.toHaveURL(/\/agents$/);
  await page.waitForTimeout(500);
  expect(sent.some((s) => s.body?.action === "close")).toBe(false);
  // ...and its close, with the frame it showed, goes with the next snapshot request.
  await page.goto("/agents");
  await expect(page.locator('ul[data-list="live"] > li.ms-row').first()).toBeVisible();
  await expect.poll(() => sent.filter((s) => s.body?.action === "close").length).toBe(1);
  const visits = async () =>
    (
      (await (await context.request.get("/api/mock/attention/visits")).json()) as {
        visits: { exposure: string; rendered_revision: number | null; closed_revision: number | null; choice: string | null; position: number | null }[];
      }
    ).visits;
  await expect.poll(async () => (await visits()).find((v) => v.choice === "fam-docs")).toMatchObject({ choice: "fam-docs", position: shown.indexOf("fam-docs") });
  const left = sent.find((s) => s.body?.action === "close")?.body?.exposure;
  expect(left).toBeTruthy();
  await expect.poll(async () => (await visits()).find((v) => v.exposure === left)).toMatchObject({ rendered_revision: 1, closed_revision: 1, choice: null });
  expect(second).toBeTruthy();
  // A whole page left (not just the list): kept in the browser, closed with the next snapshot request.
  const third = sent.filter((s) => s.method === "GET" && s.mode === "arrival").length;
  await page.goto("/agents/programs");
  await page.goto("/agents");
  await expect(page.locator('ul[data-list="live"] > li.ms-row').first()).toBeVisible();
  await expect.poll(() => sent.filter((s) => s.method === "GET" && s.mode === "arrival").length).toBeGreaterThan(third);
  await expect.poll(() => sent.filter((s) => s.body?.action === "close").length).toBe(2);
});

test("mc_attention: your own actions move only the rows they acted on, in the same visit", async ({ page }) => {
  const sent = watch(page);
  await open(page);
  const arrivals = () => sent.filter((s) => s.method === "GET" && s.mode === "arrival").length;
  await expect.poll(arrivals).toBe(1);
  const before = await ids(page);
  await row(page, "fam-tests").scrollIntoViewIfNeeded();
  await menu(page, "fam-tests", "Pin");
  await expect(row(page, "fam-tests").getByRole("img", { name: "Pinned" })).toBeVisible();
  const pin = sent.find((s) => s.body?.action === "pin");
  expect(pin?.mode, "an own action refreshes this visit").toBe("refresh");
  const others = (list: string[]) => list.filter((id) => id !== "fam-tests");
  await expect.poll(async () => others(await ids(page))).toEqual(others(before));
  // An answer is your own action too: the row answered goes on, nothing else moves. (Taken here, so
  // the mock's own request stays for the next test.)
  await page.route("**/api/harness/sessions/fam-migr/command", (route) => route.fulfill({ json: {} }));
  const migr = row(page, "fam-migr");
  await migr.scrollIntoViewIfNeeded();
  const now = await ids(page);
  await migr.getByRole("button", { name: "Allow", exact: true }).click();
  await expect(migr.locator(".ms-word")).not.toHaveText("Needs you");
  const rest = (list: string[]) => list.filter((id) => id !== "fam-migr");
  await expect.poll(async () => rest(await ids(page))).toEqual(rest(now));
  expect(arrivals(), "no new order was drawn").toBe(1);
  expect(
    sent.every((s) => s.mode !== "arrival" || s.method === "GET"),
    "no action asked for a new draw",
  ).toBe(true);
});
