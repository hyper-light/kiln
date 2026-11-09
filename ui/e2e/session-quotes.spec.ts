/** W21-R02: a reply follows its published source without rerendering the rest of the history.
 * These are DOM semantics under ordinary motion, separate from the optional render diagnostic. */
import { gunzipSync } from "node:zlib";
import { expect, test, type Page } from "@playwright/test";

type Event = { id: string; seq: number; at: number; kind: string; text: string; streaming?: boolean; reply_to?: string };
type Frame = { t: string; events?: Event[]; [key: string]: unknown };
const T = Date.UTC(2026, 10, 8);

/** More than the initial timeline window: the quoted source is not a mounted message. */
function history(source = "answer-0"): Event[] {
  return Array.from({ length: 60 }, (_, i) => [
    { id: `prompt-${i}`, seq: 10 * i + 1, at: T + i * 1000, kind: "user", text: `History prompt ${i}.`, ...(i === 58 ? { reply_to: source } : {}) },
    { id: `answer-${i}`, seq: 10 * i + 2, at: T + i * 1000 + 500, kind: "assistant", text: `History answer ${i}.`, ...(i === 59 ? { streaming: true } : {}) },
  ]).flat();
}

/** Keep the mock's real metadata and state, replace only its transcript, then own subsequent frames. */
async function transcript(page: Page, events: Event[]) {
  let send: ((frame: Frame) => void) | null = null;
  await page.routeWebSocket(
    (url) => url.pathname === "/api/harness/sessions/tree-lead/ws",
    (ws) => {
      const server = ws.connectToServer();
      let complete = false;
      let added = false;
      send = (frame) => ws.send(JSON.stringify(frame));
      server.onMessage((message) => {
        if (complete) return;
        const text = typeof message === "string" ? message : gunzipSync(message).toString("utf8");
        for (const line of text.split("\n").filter(Boolean)) {
          const frame = JSON.parse(line) as Frame;
          if (frame.t === "events") {
            if (added) continue;
            frame.events = events;
            added = true;
          }
          if (frame.t === "synced") {
            if (!added) ws.send(JSON.stringify({ t: "events", events }));
            complete = true;
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

test.beforeEach(() => {
  test.skip(!!process.env.E2E_BASE_URL, "controlled snapshots of the mock's sessions");
});

test("W21-R02 an unchanged reply follows off-window stream and replacement updates while preserving its controls", async ({ page }) => {
  const errors: string[] = [];
  page.on("pageerror", (error) => errors.push(String(error)));
  const events = history();
  const send = await transcript(page, events);
  await page.goto("/agents/tree-lead");
  const field = page.getByRole("textbox", { name: "Message to the agent", exact: true });
  await expect(field).toBeEnabled({ timeout: 30_000 });
  expect(await page.evaluate(() => matchMedia("(prefers-reduced-motion: reduce)").matches)).toBe(false);
  const reply = page.locator('[data-ev="prompt-58"]');
  const quote = reply.locator(".hx-quote");
  await expect(quote).toContainText("History answer 0.");
  await expect(page.locator('[data-ev="answer-0"]')).toHaveCount(0);
  const keptReply = await reply.elementHandle();
  const keptField = await field.elementHandle();
  await field.fill("Keep this draft and its selection while history streams.");
  await field.focus();
  await field.evaluate((el) => (el as HTMLTextAreaElement).setSelectionRange(5, 10));
  send({ t: "stream", id: "answer-0", text: " The quoted source grew." });
  await expect(quote).toContainText("History answer 0. The quoted source grew.");
  send({ t: "events", events: [{ ...events[1]!, text: "The old source was replaced with **new words**." }] });
  await expect(quote).toContainText("The old source was replaced with new words.");
  await expect(quote.locator("strong")).toHaveText("new words");
  send({ t: "stream", id: "answer-59", text: "\n\nA new streamed paragraph.\n\n```ts\nconst kept = true;\n```" });
  await expect(page.locator('[data-ev="answer-59"]')).toContainText("const kept = true;");
  await expect(quote).toContainText("The old source was replaced with new words.");
  await expect(field).toHaveValue("Keep this draft and its selection while history streams.");
  await expect(field).toBeFocused();
  expect(await keptReply?.evaluate((el) => el.isConnected)).toBe(true);
  expect(await keptField?.evaluate((el) => el.isConnected)).toBe(true);
  expect(await field.evaluate((el) => [(el as HTMLTextAreaElement).selectionStart, (el as HTMLTextAreaElement).selectionEnd])).toEqual([5, 10]);
  const long = "**Expanded quote source.** " + "Its full contents remain available. ".repeat(8);
  send({ t: "events", events: [{ ...events[1]!, text: long }] });
  await expect(quote).toContainText("the whole message");
  await quote.click();
  const viewer = page.locator(".hx-quote-full");
  await expect(viewer).toBeVisible();
  const keptViewer = await viewer.elementHandle();
  send({ t: "stream", id: "answer-0", text: " The source changed while its full quote stayed open." });
  await expect(viewer).toContainText("The source changed while its full quote stayed open.");
  await expect(viewer.locator("strong")).toHaveText("Expanded quote source.");
  expect(await keptViewer?.evaluate((el) => el.isConnected), "the expanded quote keeps its identity").toBe(true);
  await viewer.getByRole("button", { name: "Close", exact: true }).click();
  await expect(viewer).toHaveCount(0);
  expect(errors).toEqual([]);
});

test("W21-R02 a late or reordered quote follows its id, disappears on removal, and history keeps its reading anchor", async ({ page }) => {
  const errors: string[] = [];
  page.on("pageerror", (error) => errors.push(String(error)));
  const events = history("late-source");
  const send = await transcript(page, events);
  await page.goto("/agents/tree-lead");
  const field = page.getByRole("textbox", { name: "Message to the agent", exact: true });
  await expect(field).toBeEnabled({ timeout: 30_000 });
  const reply = page.locator('[data-ev="prompt-58"]');
  const quote = reply.locator(".hx-quote");
  await expect(reply).toContainText("History prompt 58.");
  await expect(quote).toHaveCount(0);
  const source: Event = { id: "late-source", seq: 3, at: T + 600, kind: "assistant", text: "The missing quoted source arrived." };
  send({ t: "events", events: [source] });
  await expect(quote).toContainText(source.text);
  // A replacement snapshot can give the same id a different position and arrive unsorted.
  send({ t: "reset" });
  send({ t: "events", events: [...events, { ...source, seq: 83, text: "The same source moved in history and changed." }].reverse() });
  send({ t: "synced" });
  await expect(quote).toContainText("The same source moved in history and changed.");
  const keptReply = await reply.elementHandle();
  await field.fill("A draft survives a replacement snapshot.");
  send({ t: "reset" });
  send({ t: "events", events });
  send({ t: "synced" });
  await expect(quote).toHaveCount(0);
  await expect(field).toHaveValue("A draft survives a replacement snapshot.");
  expect(await keptReply?.evaluate((el) => el.isConnected)).toBe(true);
  // Read in the middle of the mounted history. New paragraphs at its tail must leave that row still.
  const anchor = page.locator('[data-ev="prompt-45"]');
  await anchor.evaluate((el) => el.scrollIntoView({ block: "start" }));
  await painted(page);
  const top = await anchor.evaluate((el) => el.getBoundingClientRect().top);
  send({ t: "stream", id: "answer-59", text: "\n\n" + "A paragraph arriving below the reading position.\n\n".repeat(12) + "Anchor tail complete." });
  await expect(page.locator('[data-ev="answer-59"]')).toContainText("Anchor tail complete.");
  await painted(page);
  expect(Math.abs((await anchor.evaluate((el) => el.getBoundingClientRect().top)) - top), "the event being read keeps its offset").toBeLessThanOrEqual(2);
  expect(errors).toEqual([]);
});
