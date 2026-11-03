/**
 * W21-R15: Send never waits for a message's files. Pressed while one still uploads (held here until
 * the test lets it go), the message box is free at once and the message waits above it, saying its
 * file is on its way; once the file's up, the message goes with it. Mission control's Start doesn't
 * wait for them either: the new agent's page opens at once, and its first message goes with the
 * file once it's up. Against the mock, on a desktop and a phone.
 */
import { expect, test, type Page } from "@playwright/test";

const REAL = !!process.env.E2E_BASE_URL;
test.skip(REAL, "the mock's agents");

/** Holds every upload until the test calls what this returns. */
async function holdUploads(page: Page): Promise<() => void> {
  let release = () => {};
  const released = new Promise<void>((resolve) => (release = resolve));
  await page.route("**/api/harness/files**", async (route) => {
    if (route.request().method() !== "POST") return route.continue();
    await released;
    return route.continue();
  });
  return release;
}

type Sent = { text?: string; attachments?: { name: string }[] };

/** Every message the page sends on an agent's socket, as it sends it. */
function sentMessages(page: Page): Sent[] {
  const out: Sent[] = [];
  page.on("websocket", (ws) =>
    ws.on("framesent", (f) => {
      let frame: { t?: string; type?: string; c?: Sent & { type?: string } } & Sent;
      try {
        frame = JSON.parse(String(f.payload)) as typeof frame;
      } catch {
        return;
      }
      const c = frame.t === "cmd" ? frame.c : frame;
      if (c && (c as { type?: string }).type === "send") out.push(c);
    }),
  );
  return out;
}

/** The names of the files each message saying `text` carried. */
const carried = (sent: Sent[], text: string) => sent.filter((c) => c.text === text).map((c) => (c.attachments ?? []).map((a) => a.name));

const notes = { name: "notes.txt", mimeType: "text/plain", buffer: Buffer.from("What the agent should read first") };

test("w21_r15 Send goes while a file uploads: the box is free at once, and the message waits for its file and goes with it", async ({ page }) => {
  const release = await holdUploads(page);
  const sent = sentMessages(page);
  // The mock's agent whose harness takes files (its long conversation).
  await page.goto("/agents/demo-long");
  await expect(page.locator(".hx-agent.ag-ready")).toBeVisible({ timeout: 15_000 });
  const dock = page.locator(".hx-dock");
  const field = page.getByRole("textbox", { name: "Message to the agent" });
  await expect(field).toBeEnabled();
  await dock.locator(".cp-bar input[type=file]").setInputFiles(notes);
  await expect(dock.locator(".hx-compose-box .hx-attach-chip").filter({ hasText: "Uploading 1…" })).toBeVisible();
  const said = `Read the notes first (${Date.now()})`;
  await field.fill(said);
  const send = dock.locator(".cp-send");
  await expect(send).toBeEnabled();
  await send.click();
  // At once: the box is empty and free for what's next; the message waits above it for its file.
  await expect(field).toHaveValue("");
  await expect(dock.locator(".hx-compose-box .hx-attach-chip")).toHaveCount(0);
  const kept = dock.locator(".qd").filter({ hasText: "Kept here" });
  await expect(kept.locator(".qd-label")).toHaveText("Kept here · 1");
  await kept.locator(".qd-line").click();
  await expect(kept.getByText("Uploading notes.txt…")).toBeVisible();
  expect(carried(sent, said), "nothing goes before its file is up").toEqual([]);
  // Its file up: it goes, carrying it.
  release();
  await expect(kept).toHaveCount(0);
  await expect.poll(() => carried(sent, said)).toEqual([["notes.txt"]]);
});

test("w21_r15 Start goes while a file uploads: the new agent's page opens at once, and its first message goes with the file once it's up", async ({ page }) => {
  const release = await holdUploads(page);
  const sent = sentMessages(page);
  await page.goto("/agents");
  const box = page.getByRole("textbox", { name: "Its first message" });
  await expect(box).toBeVisible();
  const said = `Summarize these notes (${Date.now()})`;
  await box.fill(said);
  await page.locator(".ms-msg input[type=file]").setInputFiles(notes);
  await expect(page.locator(".ms-msg .hx-attach-chip").filter({ hasText: "Uploading 1…" })).toBeVisible();
  const start = page.locator(".mc-start");
  await expect(start).toBeEnabled({ timeout: 15_000 });
  await start.click();
  // Its page, with the upload still held: Start didn't wait for it.
  await expect(page).toHaveURL(/\/agents\/[0-9a-f]{32}$/);
  await expect(page.locator(".hx-timeline .hx-prompt-text").filter({ hasText: said })).toBeVisible();
  release();
  await expect(page.locator(".hx-agent.ag-ready")).toBeVisible({ timeout: 15_000 });
  await expect.poll(() => carried(sent, said), { timeout: 15_000 }).toEqual([["notes.txt"]]);
});
