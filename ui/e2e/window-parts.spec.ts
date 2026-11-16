/**
 * W21-R02: a conversation opens on its window's first part (docs/wire.md, "A window in two parts"): its newest
 * messages show as soon as that part is in, and the rest of the window joins above them with nothing on the screen
 * moving. A part too short to fill the pane isn't shown ahead of the rest (it would move when the rest came): the
 * conversation's loader stays until the window is whole.
 */
import { gunzipSync } from "node:zlib";
import { expect, test, type Page } from "@playwright/test";
import type { HEvent } from "../src/api/harness";

const T = Date.UTC(2026, 10, 13);
type Frame = { t: string; [key: string]: unknown };
const CURSOR = "77.1.tree-lead";
const lines = (n: number, of: string) => Array.from({ length: n }, (_, i) => `${of}, line ${i + 1}: the retry loop reads the header, sleeps for what it says and then asks again.`).join("\n\n");
const ask = (seq: number, text: string): HEvent => ({ id: `part-ask-${seq}`, seq, at: T + seq, kind: "user", text });
const answer = (seq: number, text: string): HEvent => ({ id: `part-answer-${seq}`, seq, at: T + seq, kind: "assistant", text });

/** Six older turns (the window's rest), then its first part: the newest two turns, long ones (`tall`), or the newest alone, a line. */
function conversation(tall: boolean) {
  const rest: HEvent[] = [];
  for (let turn = 0; turn < 6; turn++) rest.push(ask(100 + turn * 2, `Older question ${turn + 1}`), answer(101 + turn * 2, lines(6, `Older answer ${turn + 1}`)));
  const part = tall
    ? [ask(200, "Newest question but one"), answer(201, lines(14, "Newest answer but one")), ask(202, "The newest question"), answer(203, lines(14, "The newest answer"))]
    : [ask(202, "The newest question"), answer(203, "The newest answer, in a line.")];
  return { rest, part };
}

/**
 * The session's socket, its snapshot the first part of `c`'s window, as a server sends one to a viewer that takes a part
 * ahead (`first_bytes`). What it returns sends the rest (`earlier`), when the test says.
 */
async function parted(page: Page, c: ReturnType<typeof conversation>) {
  let send: ((frame: Frame) => void) | null = null;
  let asked = "";
  await page.routeWebSocket(
    (url) => url.pathname === "/api/harness/sessions/tree-lead/ws",
    (ws) => {
      asked = ws.url();
      const server = ws.connectToServer();
      let done = false;
      let supplied = false;
      send = (frame) => ws.send(JSON.stringify(frame));
      server.onMessage((message) => {
        if (done) return;
        const text = typeof message === "string" ? message : gunzipSync(message).toString("utf8");
        for (const line of text.split("\n").filter(Boolean)) {
          const frame = JSON.parse(line) as Frame;
          if (frame.t === "history" || frame.t === "earlier") continue;
          if (frame.t === "open") frame.window = { before: c.part[0]?.seq, more: true };
          if (frame.t === "events") {
            if (supplied) continue;
            ws.send(JSON.stringify({ t: "history", cursor: CURSOR, before: c.part[0]?.seq, has_more: true, oversized: false, part: true }));
            frame.events = c.part;
            supplied = true;
          }
          if (frame.t === "synced") done = true;
          ws.send(JSON.stringify(frame));
        }
      });
      ws.onMessage((message) => server.send(message));
    },
  );
  return {
    asked: () => asked,
    rest: () => {
      if (!send) throw new Error("The session's socket has not opened");
      send({ t: "earlier", events: c.rest, cursor: CURSOR, before: c.rest[0]?.seq, has_more: true, oversized: false });
    },
  };
}

const painted = (page: Page) => page.evaluate(() => new Promise<void>((resolve) => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
/** How far the timeline is from its end, in points. */
const fromEnd = (page: Page) => page.locator(".hx-timeline").evaluate((el) => el.scrollHeight - el.scrollTop - el.clientHeight);

test.beforeEach(() => test.skip(!!process.env.E2E_BASE_URL, "a controlled session's socket"));

test("W21-R02 a conversation shows on its window's first part, and the rest joins above with nothing moving", async ({ page }) => {
  const errors: string[] = [];
  page.on("pageerror", (error) => errors.push(error.message));
  const c = conversation(true);
  const socket = await parted(page, c);
  await page.goto("/agents/tree-lead");
  // The first part alone: its newest messages, at the conversation's end.
  const newest = page.locator('.hx-outcome[data-ev="part-answer-203"]');
  await expect(newest).toBeVisible({ timeout: 30_000 });
  expect(socket.asked()).toContain("first_bytes=16384");
  await expect(page.getByText("Older question 1", { exact: true })).toHaveCount(0);
  await expect(page.getByRole("button", { name: "Earlier messages" })).toHaveCount(0);
  await expect(page.locator(".hx-part-loading")).toBeHidden();
  await painted(page);
  expect(await fromEnd(page)).toBeLessThan(2);
  // Where what's on the screen is, at every frame from here until the rest is in.
  await page.evaluate(() => {
    const seen = ((window as unknown as { __tops: number[] }).__tops = [] as number[]);
    const el = document.querySelector('[data-ev="part-ask-202"]');
    const tick = () => {
      if (el) seen.push(el.getBoundingClientRect().top);
      requestAnimationFrame(tick);
    };
    requestAnimationFrame(tick);
  });
  await painted(page);
  socket.rest();
  await expect(page.getByText("Older question 6", { exact: true })).toBeAttached();
  await painted(page);
  await painted(page);
  const tops = await page.evaluate(() => (window as unknown as { __tops: number[] }).__tops);
  expect(tops.length).toBeGreaterThan(3);
  expect(Math.max(...tops) - Math.min(...tops), `the newest question's place at each of ${tops.length} frames`).toBeLessThan(1);
  expect(await fromEnd(page)).toBeLessThan(2);
  // The window is whole: what's before it is a page away again, and its oldest is above.
  await expect(page.getByRole("button", { name: "Earlier messages" })).toBeAttached();
  await expect(page.getByText("Older question 1", { exact: true })).toBeAttached();
  expect(errors).toEqual([]);
});

test("W21-R02 a first part too short to fill the pane waits behind the conversation's loader for the rest", async ({ page }) => {
  const c = conversation(false);
  const socket = await parted(page, c);
  await page.goto("/agents/tree-lead");
  const loader = page.locator(".hx-timeline .hx-part-loading");
  await expect(loader).toBeVisible({ timeout: 30_000 });
  await expect(loader).toContainText("Loading conversation");
  // What's in already isn't shown: it sits at the top of the pane, and would move down as the rest came in above it.
  const newest = page.locator('.hx-outcome[data-ev="part-answer-203"]');
  await expect(newest).toBeAttached();
  await expect(newest).toBeHidden();
  socket.rest();
  await expect(newest).toBeVisible();
  await expect(loader).toHaveCount(0);
  await painted(page);
  expect(await fromEnd(page)).toBeLessThan(2);
  await expect(page.getByText("Older question 1", { exact: true })).toBeAttached();
});
