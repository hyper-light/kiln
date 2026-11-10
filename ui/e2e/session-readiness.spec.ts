/** W21-R02: a session's first snapshot stays atomic while its readiness owner remains mounted.
 * Main conversations, subagent threads and inline questions retain their identity and controls.
 * These are semantic checks against the mock; the production benchmark measures reveal latency. */
import { gunzipSync } from "node:zlib";
import { expect, test, type Page } from "@playwright/test";

const REAL = !!process.env.E2E_BASE_URL;
type Frame = { t?: string; id?: string; c?: { type?: string; text?: string }; events?: { seq?: number; [key: string]: unknown }[] };

/** Forward the real snapshot's parts, holding its completion and anything after it until released. */
async function holdSnapshot(page: Page, id: string) {
  const marker = `Readiness snapshot belongs to ${id}.`;
  const commands: Frame[] = [];
  let connections = 0;
  let captured = () => {};
  const complete = new Promise<void>((resolve) => (captured = resolve));
  let release = () => {};
  await page.routeWebSocket(
    (url) => url.pathname === `/api/harness/sessions/${id}/ws`,
    (ws) => {
      connections++;
      const server = ws.connectToServer();
      const pending: string[] = [];
      let held = false;
      release = () => {
        held = false;
        for (const line of pending.splice(0)) ws.send(line);
      };
      server.onMessage((message) => {
        const text = typeof message === "string" ? message : gunzipSync(message).toString("utf8");
        for (const line of text.split("\n").filter(Boolean)) {
          const frame = JSON.parse(line) as Frame;
          if (frame.t === "events") {
            const seq = Math.max(0, ...(frame.events ?? []).map((e) => e.seq ?? 0)) + 1;
            frame.events = [...(frame.events ?? []), { id: `readiness-${id}`, seq, at: Date.now(), kind: "assistant", text: marker }];
          }
          if (frame.t === "synced") held = true;
          if (held) pending.push(JSON.stringify(frame));
          else ws.send(JSON.stringify(frame));
          if (frame.t === "synced") captured();
        }
      });
      ws.onMessage((message) => {
        const frame = JSON.parse(String(message)) as Frame;
        if (frame.t === "cmd" && frame.c?.type === "send") {
          commands.push(frame);
          // Observe the destination without changing a shared mock conversation.
          ws.send(JSON.stringify({ t: "ack", id: frame.id, outcome: "taken" }));
        } else server.send(message);
      });
    },
  );
  return { marker, commands, complete, release: () => release(), connections: () => connections };
}

test.beforeEach(() => {
  test.skip(REAL, "controlled snapshots of the mock's sessions");
});

test("W21-R02 the main conversation publishes only a complete snapshot and preserves its draft and send destination", async ({ page }) => {
  const held = await holdSnapshot(page, "tree-lead");
  const draft = "The draft for Storage lead survives readiness.";
  await page.addInitScript((text) => sessionStorage.setItem("kiln.agents.drafts", JSON.stringify({ state: { drafts: { "tree-lead": text } }, version: 0 })), draft);
  await page.goto("/agents/tree-lead");
  await held.complete;
  const field = page.getByRole("textbox", { name: "Message to the agent", exact: true });
  await expect(field).toBeDisabled();
  await expect(field).toHaveValue(draft);
  await expect(page.getByText(held.marker, { exact: true })).toHaveCount(0);
  held.release();
  await expect(page.getByText(held.marker, { exact: true })).toBeVisible();
  await expect(field).toBeEnabled();
  await expect(field).toHaveValue(draft);
  await expect(page.locator(".ag-title")).toHaveText("Storage lead");
  await field.fill("Send this only to Storage lead.");
  await page.locator(".hx-dock").getByRole("button", { name: "Send", exact: true }).click();
  await expect.poll(() => held.commands.map((f) => f.c?.text)).toEqual(["Send this only to Storage lead."]);
  expect(held.connections()).toBe(1);
});

test("W21-R02 a subagent thread waits for its own complete snapshot and sends through its own store", async ({ page }) => {
  const held = await holdSnapshot(page, "tree-s1");
  await page.goto("/agents/tree-lead?task=tree-s1");
  await held.complete;
  await expect(page.getByText(held.marker, { exact: true })).toHaveCount(0);
  await expect(page.getByRole("navigation", { name: "Path" })).toContainText("Leader election over QUIC");
  held.release();
  await expect(page.getByText(held.marker, { exact: true })).toBeVisible();
  const field = page.getByRole("textbox", { name: "Message to the agent", exact: true });
  await expect(field).toBeEnabled();
  await field.fill("Send this only to the election subagent.");
  await page.locator(".hx-dock").getByRole("button", { name: "Queue", exact: true }).click();
  await expect.poll(() => held.commands.map((f) => f.c?.text)).toEqual(["Send this only to the election subagent."]);
  expect(held.connections()).toBe(1);
});

test("W21-R02 an inline question remains under its row while its own snapshot becomes ready", async ({ page }) => {
  await page.request.post("/api/mock/asks");
  const held = await holdSnapshot(page, "ask-s2");
  await page.goto("/agents");
  const row = page.locator('.ms-row[data-agent="ask-lead"]');
  await row.getByRole("button", { name: "Answer", exact: true }).click();
  await held.complete;
  await expect(row.locator(".ms-answer-wait")).toBeVisible();
  await expect(row.getByRole("radio", { name: "Observe's first" })).toHaveCount(0);
  held.release();
  await expect(row.getByRole("radio", { name: "Observe's first" })).toBeVisible();
  await expect(page).toHaveURL(/\/agents$/);
  expect(held.connections()).toBe(1);
});
