/** W21-R02: actual row pages keep find, selected DOM and saved places on one canonical snapshot. */
import { gunzipSync } from "node:zlib";
import { expect, test, type Page } from "@playwright/test";
import type { HEvent } from "../src/api/harness";

const T = Date.UTC(2026, 10, 9);
type Frame = { t: string; [key: string]: unknown };
const prompt: HEvent = { id: "window-prompt", seq: 1, at: T, kind: "user", text: "Inspect every command, preserving the reading position." };
const call = (n: number): HEvent => ({
  id: `window-call-${n}`,
  seq: n + 2,
  at: T + n + 1,
  kind: "tool",
  tool_id: `window-tool-${n}`,
  name: "Bash",
  tool: "shell",
  title: `printf unique-command-${n}`,
  input: {},
  status: n === 10 ? "pending" : n === 59 ? "error" : "done",
  started: T + n,
  ended: T + n + 1,
  exit_code: n === 59 ? 1 : 0,
  output: `Complete output from command ${n}.`,
});

async function controlled(page: Page, events: HEvent[], history?: { cursor: string; before: number; has_more: boolean; oversized: boolean }) {
  let send: ((frame: Frame) => void) | null = null;
  await page.routeWebSocket(
    (url) => url.pathname === "/api/harness/sessions/tree-lead/ws",
    (ws) => {
      const server = ws.connectToServer();
      let done = false;
      let supplied = false;
      send = (frame) => ws.send(JSON.stringify(frame));
      server.onMessage((message) => {
        if (done) return;
        const text = typeof message === "string" ? message : gunzipSync(message).toString("utf8");
        for (const line of text.split("\n").filter(Boolean)) {
          const frame = JSON.parse(line) as Frame;
          if (frame.t === "history" && history) continue;
          if (history && frame.t === "open") frame.window = { before: history.before, more: history.has_more };
          if (history && frame.t === "state") frame.state = { ...(frame.state as Record<string, unknown>), dropped: 100 };
          if (frame.t === "events") {
            if (supplied) continue;
            // Bounded metadata belongs to the snapshot prefix, before its events.
            if (history) ws.send(JSON.stringify({ t: "history", ...history }));
            frame.events = events;
            supplied = true;
          }
          if (frame.t === "synced") {
            done = true;
          }
          ws.send(JSON.stringify(frame));
        }
      });
      ws.onMessage((message) => server.send(message));
    },
  );
  return (frame: Frame) => {
    if (!send) throw new Error("The controlled session has not opened");
    send(frame);
  };
}

const painted = (page: Page) => page.evaluate(() => new Promise<void>((resolve) => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
test.beforeEach(() => test.skip(!!process.env.E2E_BASE_URL, "controlled mock transcripts"));

test("W21-R02 a grouped step preserves focused expanded controls, selection and its concrete row anchor while60 becomes2000 calls", async ({ page }) => {
  const errors: string[] = [];
  page.on("pageerror", (error) => errors.push(error.message));
  const events = [prompt, ...Array.from({ length: 60 }, (_, n) => call(n))];
  const send = await controlled(page, events);
  await page.goto("/agents/tree-lead");
  await expect(page.getByRole("textbox", { name: "Message to the agent", exact: true })).toBeEnabled({ timeout: 30_000 });
  const row = page.locator('[data-window-row="call"][data-ev="window-call-20"]');
  const head = row.locator(".hx-cmd-head");
  await expect(row).toBeAttached();
  await head.click();
  await expect(head).toHaveAttribute("aria-expanded", "true");
  await row.evaluate((el) => el.scrollIntoView({ block: "start" }));
  await painted(page);
  const keptRow = await row.elementHandle();
  const keptHead = await head.elementHandle();
  const top = await row.evaluate((el) => el.getBoundingClientRect().top);
  await row.locator(".hx-cmd-line").evaluate((el) => {
    const range = document.createRange();
    range.selectNodeContents(el);
    const selection = document.getSelection();
    selection?.removeAllRanges();
    selection?.addRange(range);
  });
  await head.focus();
  events.push(call(60));
  send({ t: "events", events: [call(60)] });
  await expect(page.locator('[data-window-row="call"][data-ev="window-call-60"]')).toBeAttached();
  const later = Array.from({ length: 1939 }, (_, n) => call(n + 61));
  events.push(...later);
  send({ t: "events", events: later });
  await expect(page.locator('[data-window-row="call"][data-ev="window-call-1999"]')).toBeAttached();
  await expect(page.locator('[data-window-row="call"][data-ev="window-call-10"]')).toBeAttached();
  await painted(page);
  await expect(head).toBeFocused();
  await expect(head).toHaveAttribute("aria-expanded", "true");
  expect(await keptRow?.evaluate((el) => el.isConnected)).toBe(true);
  expect(await keptHead?.evaluate((el) => el.isConnected)).toBe(true);
  expect(await page.evaluate(() => document.getSelection()?.toString())).toContain("unique-command-20");
  expect(Math.abs((await row.evaluate((el) => el.getBoundingClientRect().top)) - top)).toBeLessThanOrEqual(2);
  expect(await page.locator('[data-window-row="call"]').count()).toBeLessThanOrEqual(63);
  // Save a concrete inner-row place; reload reopens just its containing step and restores it.
  await row.evaluate((el) => el.scrollIntoView({ block: "start" }));
  await expect.poll(() => page.evaluate(() => JSON.parse(sessionStorage.getItem("kiln.agents.places") ?? "{}").state?.places?.["tree-lead"]?.row)).toBe(true);
  const saved = await page.evaluate(() => JSON.parse(sessionStorage.getItem("kiln.agents.places") ?? "{}").state.places["tree-lead"]);
  expect(saved.neighbors?.length ?? 0).toBeLessThanOrEqual(2);
  events[60] = { ...call(59), status: "done", exit_code: 0 } as HEvent;
  await page.reload({ waitUntil: "domcontentloaded" });
  await expect(page.getByRole("textbox", { name: "Message to the agent", exact: true })).toBeEnabled({ timeout: 30_000 });
  await expect(row).toBeVisible();
  await painted(page);
  expect(Math.abs((await row.evaluate((el) => el.getBoundingClientRect().top)) - top)).toBeLessThanOrEqual(2);
  expect(errors).toEqual([]);
});

test("W21-R02 finding an earlier row of a giant turn opens only its target page and keeps every later step reachable", async ({ page }) => {
  const events: HEvent[] = [
    prompt,
    ...Array.from({ length: 1000 }, (_, n): HEvent => ({ id: `window-note-${n}`, seq: n + 2, at: T + n + 1, kind: "assistant", text: `Unique note marker ${n}.`, streaming: n === 999 })),
  ];
  await controlled(page, events);
  await page.goto("/agents/tree-lead");
  await expect(page.getByRole("textbox", { name: "Message to the agent", exact: true })).toBeEnabled({ timeout: 30_000 });
  await expect(page.locator('.hx-work > [data-ev="window-note-10"]')).toHaveCount(0);
  await page.keyboard.press("ControlOrMeta+f");
  const find = page.getByRole("textbox", { name: "Find in this conversation", exact: true });
  await find.fill("Unique note marker 10.");
  await expect(page.locator('.hx-work > [data-ev="window-note-10"]')).toBeVisible();
  expect(await page.locator(".hx-work > .hx-said").count()).toBeLessThanOrEqual(61);
  const later = page.locator(".hx-work > .hx-earlier-steps").filter({ hasText: "later steps" });
  await expect(later).toHaveCount(1);
  await later.click();
  expect(await page.locator(".hx-work > .hx-said").count()).toBeLessThanOrEqual(62);
  await expect(page.locator('.hx-work > [data-ev="window-note-70"]')).toBeAttached();
  expect(await page.evaluate(() => matchMedia("(prefers-reduced-motion: reduce)").matches)).toBe(false);
});

async function heldOlderPlace(page: Page) {
  const cursor = "restore-fixture-generation";
  await page.addInitScript(() => {
    sessionStorage.setItem(
      "kiln.agents.places",
      JSON.stringify({ state: { places: { "tree-lead": { id: "window-call-20", seq: 22, row: true, offset: 0, neighbors: [{ id: "window-call-21", row: true }], at: Date.now() } } }, version: 0 }),
    );
    const audit = { released: false, tailFlashed: false, signals: 0, aborted: 0, rejections: [] as string[] };
    Object.assign(window, { __restoreAudit: audit });
    const fetch = window.fetch.bind(window);
    window.fetch = (input, init) => {
      const path = new URL(input instanceof Request ? input.url : String(input), location.href).pathname;
      if (path !== "/api/harness/sessions/tree-lead/events") return fetch(input, init);
      const signal = init?.signal ?? (input instanceof Request ? input.signal : null);
      if (signal) {
        audit.signals++;
        const aborted = () => audit.aborted++;
        if (signal.aborted) aborted();
        else signal.addEventListener("abort", aborted, { once: true });
      }
      return fetch(input, init).catch((error: unknown) => {
        audit.rejections.push(error instanceof DOMException || error instanceof Error ? error.name : String(error));
        throw error;
      });
    };
    new MutationObserver(() => {
      if (!audit.released && document.querySelector(".hx-flow > .hx-turn")) audit.tailFlashed = true;
    }).observe(document, { childList: true, subtree: true });
  });
  await controlled(
    page,
    Array.from({ length: 60 }, (_, n) => call(n + 100)),
    { cursor, before: 102, has_more: true, oversized: false },
  );
  let requests = 0;
  let release = () => {};
  const held = new Promise<void>((resolve) => (release = resolve));
  const failed: string[] = [];
  page.on("requestfailed", (request) => {
    if (new URL(request.url()).pathname === "/api/harness/sessions/tree-lead/events") failed.push(request.failure()?.errorText ?? "failed");
  });
  await page.route("**/api/harness/sessions/tree-lead/events?*", async (route) => {
    requests++;
    const query = new URL(route.request().url()).searchParams;
    expect(query.get("cursor")).toBe(cursor);
    expect(query.get("before")).toBe("102");
    await held;
    try {
      await route.fulfill({ json: { events: [prompt, ...Array.from({ length: 80 }, (_, n) => call(n + 20))], history: { cursor, before: 1, has_more: false, oversized: false } } });
    } catch {
      /* Expected when Latest aborts this owned request. */
    }
  });
  return { release, requests: () => requests, failed };
}

test("W21-R02 a bounded snapshot automatically restores a saved older row through canonical HTTP history without flashing the tail", async ({ page }) => {
  const older = await heldOlderPlace(page);
  try {
    await page.goto("/agents/tree-lead");
    await expect(page.locator(".hx-restore-place")).toContainText("Restoring your saved place", { timeout: 30_000 });
    await expect.poll(older.requests).toBe(1);
    await expect(page.locator(".hx-flow > .hx-turn")).toHaveCount(0);
    expect(await page.evaluate(() => JSON.parse(sessionStorage.getItem("kiln.agents.places") ?? "{}").state.places["tree-lead"].id)).toBe("window-call-20");
    await page.evaluate(() => {
      (window as unknown as { __restoreAudit: { released: boolean } }).__restoreAudit.released = true;
    });
    older.release();
    const row = page.locator('[data-window-row="call"][data-ev="window-call-20"]');
    await expect(row).toBeVisible();
    await expect(page.locator(".hx-restore-place")).toHaveCount(0);
    await painted(page);
    const offset = await row.evaluate((el) => el.getBoundingClientRect().top - el.closest(".hx-timeline")!.getBoundingClientRect().top);
    expect(Math.abs(offset)).toBeLessThanOrEqual(2);
    expect(await page.evaluate(() => (window as unknown as { __restoreAudit: { tailFlashed: boolean } }).__restoreAudit.tailFlashed)).toBe(false);
    expect(older.requests()).toBe(1);
    // The server kept a short mirror, but the cursor now says every older page has joined.
    // Neither its original Open.window.more nor State.dropped may keep Find marked partial.
    await expect(page.getByText("100 earlier events not shown", { exact: true })).toHaveCount(0);
    await page.keyboard.press("ControlOrMeta+f");
    await page.getByRole("textbox", { name: "Find in this conversation", exact: true }).fill("unique-command-20");
    await expect(page.locator(".hx-find-where")).toHaveText("1 of 1");
    await expect(page.locator(".hx-find-partial")).toHaveCount(0);
  } finally {
    older.release();
  }
});

test("W21-R02 Latest cancels the owned restore page and preserves the live bounded snapshot against late history", async ({ page }, info) => {
  const older = await heldOlderPlace(page);
  const cancellation = () => page.evaluate(() => (window as unknown as { __restoreAudit: { signals: number; aborted: number; rejections: string[] } }).__restoreAudit);
  try {
    await page.goto("/agents/tree-lead");
    await expect(page.locator(".hx-restore-place")).toBeVisible({ timeout: 30_000 });
    await expect.poll(older.requests).toBe(1);
    await expect.poll(async () => (await cancellation()).signals).toBe(1);
    await page.getByRole("button", { name: "Latest messages", exact: true }).click();
    await expect(page.locator(".hx-restore-place")).toHaveCount(0);
    await expect(page.locator(".hx-flow > .hx-turn")).toBeVisible();
    await expect.poll(async () => (await cancellation()).aborted).toBe(1);
    await expect.poll(async () => (await cancellation()).rejections).toEqual(["AbortError"]);
    older.release();
    await painted(page);
    await expect(page.locator('[data-ev~="window-call-20"]')).toHaveCount(0);
    expect(older.requests()).toBe(1);
  } finally {
    older.release();
    // Playwright may withhold requestfailed while the route's handler is held; the actual
    // fetch's AbortSignal and AbortError above prove cancellation independently of that hook.
    await info.attach("restore-cancellation", { body: JSON.stringify({ ...(await cancellation()), requestFailed: older.failed }), contentType: "application/json" });
  }
});
