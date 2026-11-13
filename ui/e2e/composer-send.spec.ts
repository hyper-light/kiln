/**
 * The agent page's message box sends (2026-11-13: every message from it stayed in the box, for
 * every agent, while other ways of sending went). Typed and sent, a message goes on the agent's
 * socket and the box empties, whatever this browser kept from before: a fresh browser, one with
 * the message box's old saved messages, and one whose saved originals are in an older shape.
 * Against the mock, on a desktop and a phone, in Chromium and WebKit.
 */
import { expect, test, type Page } from "@playwright/test";

const REAL = !!process.env.E2E_BASE_URL;
test.skip(REAL, "the mock's agents");

/** The texts of the messages the page sends on an agent's socket, as it sends them. */
function sentTexts(page: Page): string[] {
  const out: string[] = [];
  page.on("websocket", (ws) =>
    ws.on("framesent", (f) => {
      let frame: { t?: string; type?: string; text?: string; c?: { type?: string; text?: string } };
      try {
        frame = JSON.parse(String(f.payload)) as typeof frame;
      } catch {
        return;
      }
      const c = frame.t === "cmd" ? frame.c : frame;
      if (c?.type === "send" && typeof c.text === "string") out.push(c.text);
    }),
  );
  return out;
}

async function sendFromBox(page: Page, text: string): Promise<string[]> {
  const sent = sentTexts(page);
  await page.goto("/agents/demo-long");
  await expect(page.locator(".hx-agent.ag-ready")).toBeVisible({ timeout: 15_000 });
  const field = page.getByRole("textbox", { name: "Message to the agent" });
  await expect(field).toBeEnabled();
  await field.fill(text);
  // Its send button, as a person presses it (on a phone, Enter makes a new line).
  await page
    .locator(".hx-dock")
    .getByRole("button", { name: /^(Send|Queue|Steer|Send now)$/ })
    .first()
    .click();
  await expect.poll(() => sent.includes(text), { timeout: 15_000, message: `"${text}" went on the socket` }).toBe(true);
  await expect(field).toHaveValue("");
  return sent;
}

test("composer_send a message typed in the box goes to the agent", async ({ page }) => {
  await sendFromBox(page, "first words from the box");
});

test("composer_send the box sends with the old saved messages in this browser", async ({ page }) => {
  await page.addInitScript(() => {
    // The message box's saved messages before W20 (zustand persist, version 0), one kept for an agent.
    localStorage.setItem("kiln.agents.held", JSON.stringify({ state: { held: { "demo-long": [{ id: "old-1", text: "kept from before", at: 1 }] } }, version: 0 }));
  });
  await sendFromBox(page, "after the old saved messages");
});

test("composer_send the box sends with saved originals in an older shape", async ({ page }) => {
  await page.addInitScript(() => {
    const open = indexedDB.open("kiln.agents.held", 1);
    open.onupgradeneeded = () => {
      open.result.createObjectStore("held");
      open.result.createObjectStore("legacy", { autoIncrement: true });
    };
    open.onsuccess = () => {
      const tx = open.result.transaction("held", "readwrite");
      // An original kept by an older build: a file without its bytes, and an agent's rows not a list.
      tx.objectStore("held").put({ "demo-long": [{ id: "old-2", text: "older shape", at: 2, owner: "x", originals: [{ name: "a.txt", mime: "text/plain" }] }], other: "not a list" }, "originals");
      tx.oncomplete = () => open.result.close();
    };
  });
  await sendFromBox(page, "after the older originals");
});

test("composer_send the box sends from a browser that can't keep a copy of it", async ({ page }) => {
  await page.addInitScript(() => {
    // Storage this browser won't open (a private window, a full disk, a blocked upgrade).
    IDBFactory.prototype.open = function () {
      throw new DOMException("storage unavailable", "UnknownError");
    };
  });
  await sendFromBox(page, "with no browser copy");
});
