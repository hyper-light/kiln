/** W21-R02: actual navigation intent stays data-only, cancels unused reads, then promotes the
 * same in-flight socket when clicked. Browser coverage also catches timer receiver regressions. */
import { gunzipSync } from "node:zlib";
import { expect, test } from "@playwright/test";

test("W21-R02 mission intent discovers agent code without running a loader or mounting the conversation", async ({ page }) => {
  test.skip(!!process.env.E2E_BASE_URL, "controlled mock navigation and code-split production assets");
  const chunks: string[] = [];
  const serverFunctions: string[] = [];
  const errors: string[] = [];
  page.on("pageerror", (error) => errors.push(error.message));
  page.on("requestfinished", (request) => {
    const path = new URL(request.url()).pathname;
    if (/\/assets\/_app\.agents\._id-[^/]+\.js$/.test(path)) chunks.push(path);
    if (path.startsWith("/_serverFn/")) serverFunctions.push(path);
  });
  await page.goto("/agents");
  const open = page.locator('.ms-row[data-agent="tree-lead"] .ms-go');
  await expect(open).toBeVisible({ timeout: 30_000 });
  expect(chunks).toEqual([]);
  const before = serverFunctions.length;
  await open.focus();
  await expect.poll(() => chunks.length).toBe(1);
  await expect(page).toHaveURL(/\/agents$/);
  await expect(page.locator(".ag-ready")).toHaveCount(0);
  await expect(page.getByRole("textbox", { name: "Message to the agent", exact: true })).toHaveCount(0);
  expect(serverFunctions.length).toBe(before);
  await open.click();
  await expect(page.getByRole("textbox", { name: "Message to the agent", exact: true })).toBeEnabled({ timeout: 30_000 });
  expect(chunks).toHaveLength(1);
  expect(errors).toEqual([]);
});

test("W21-R02 leaving a row cancels speculation; clicking promotes without commands or a duplicate socket", async ({ page }, info) => {
  test.skip(!!process.env.E2E_BASE_URL, "controlled snapshots of the mock's sessions");
  await page.clock.install();
  const lifetimes: { id: number; opened: number; closed?: number; closeCallbacks: number }[] = [];
  const errors: string[] = [];
  try {
    page.on("pageerror", (error) => errors.push(error.message));
    let connections = 0;
    const closed = new Set<number>();
    let completed = 0;
    let release = () => {};
    const commands: unknown[] = [];
    const marker = "The prefetched Storage lead snapshot is complete.";
    await page.routeWebSocket(
      (url) => url.pathname === "/api/harness/sessions/tree-lead/ws",
      (ws) => {
        connections++;
        const connection = connections;
        const lifetime = { id: connection, opened: performance.now(), closeCallbacks: 0 };
        lifetimes.push(lifetime);
        const server = ws.connectToServer();
        ws.onClose((code, reason) => {
          lifetime.closeCallbacks++;
          if (closed.has(connection)) return;
          Object.assign(lifetime, { closed: performance.now() });
          closed.add(connection);
          void server.close({ code, reason });
        });
        let held = false;
        const pending: string[] = [];
        release = () => {
          held = false;
          for (const frame of pending.splice(0)) ws.send(frame);
        };
        server.onMessage((message) => {
          const text = typeof message === "string" ? message : gunzipSync(message).toString("utf8");
          for (const line of text.split("\n").filter(Boolean)) {
            const frame = JSON.parse(line) as { t?: string; events?: { seq?: number; [key: string]: unknown }[] };
            if (frame.t === "events") {
              const seq = Math.max(0, ...(frame.events ?? []).map((event) => event.seq ?? 0)) + 1;
              frame.events = [...(frame.events ?? []), { id: "prefetch-marker", seq, at: Date.now(), kind: "assistant", text: marker }];
            }
            if (frame.t === "synced") held = true;
            if (held) pending.push(JSON.stringify(frame));
            else ws.send(JSON.stringify(frame));
            if (frame.t === "synced") completed++;
          }
        });
        ws.onMessage((message) => {
          const frame = JSON.parse(String(message)) as { t?: string };
          if (frame.t === "cmd") commands.push(frame);
          server.send(message);
        });
      },
    );
    await page.goto("/agents");
    const open = page.locator('.ms-row[data-agent="tree-lead"] .ms-go');
    await expect(open).toBeVisible({ timeout: 30_000 });
    await page.clock.pauseAt(new Date((await page.evaluate(() => Date.now())) + 60_000));
    await open.focus();
    await page.clock.runFor(60);
    await expect.poll(() => completed).toBe(1);
    await open.evaluate((el) => el.blur());
    await expect.poll(() => closed.size).toBe(1);
    await open.hover();
    await page.clock.runFor(60);
    await expect.poll(() => completed).toBe(2);
    expect(commands).toEqual([]);
    await expect(page).toHaveURL(/\/agents$/);
    await page.mouse.move(0, 0);
    await expect.poll(() => closed.size).toBe(2);
    await open.hover();
    await page.clock.runFor(60);
    await expect.poll(() => completed).toBe(3);
    await open.click();
    await page.clock.resume();
    await expect(page).toHaveURL(/\/agents\/tree-lead$/);
    const field = page.getByRole("textbox", { name: "Message to the agent", exact: true });
    await expect(field).toBeDisabled();
    expect(connections, "click reuses the third speculative connection").toBe(3);
    expect(commands).toEqual([]);
    release();
    await expect(page.getByText(marker, { exact: true })).toBeVisible();
    await expect(field).toBeEnabled();
    expect(closed.size, "leaving the mission row cannot close its promoted socket").toBe(2);
    expect(connections).toBe(3);
    expect(errors).toEqual([]);
  } finally {
    await info.attach("prefetch-connection-lifetimes", { body: JSON.stringify(lifetimes, null, 2), contentType: "application/json" });
  }
});

test("W21-R02 a completed prefetch shows cached history and an editable draft while its command channel reconnects", async ({ page }) => {
  test.skip(!!process.env.E2E_BASE_URL, "controlled snapshots of the mock's sessions");
  await page.clock.install();
  const errors: string[] = [];
  page.on("pageerror", (error) => errors.push(error.message));
  const draft = "The draft remains editable while a cached conversation reconnects.";
  await page.addInitScript((text) => sessionStorage.setItem("kiln.agents.drafts", JSON.stringify({ state: { drafts: { "tree-lead": text } }, version: 0 })), draft);
  const marker = "Completed cached Storage lead history.";
  let connections = 0;
  let firstClosed = false;
  let secondCaptured = false;
  let release = () => {};
  const commands: unknown[] = [];
  await page.routeWebSocket(
    (url) => url.pathname === "/api/harness/sessions/tree-lead/ws",
    (ws) => {
      const connection = ++connections;
      const server = ws.connectToServer();
      let closed = false;
      ws.onClose((code, reason) => {
        if (closed) return;
        closed = true;
        if (connection === 1) firstClosed = true;
        void server.close({ code, reason });
      });
      const pending: string[] = [];
      let held = connection > 1;
      if (held)
        release = () => {
          held = false;
          for (const frame of pending.splice(0)) ws.send(frame);
        };
      server.onMessage((message) => {
        const text = typeof message === "string" ? message : gunzipSync(message).toString("utf8");
        for (const line of text.split("\n").filter(Boolean)) {
          const frame = JSON.parse(line) as { t?: string; events?: { seq?: number; [key: string]: unknown }[] };
          if (frame.t === "events") {
            const seq = Math.max(0, ...(frame.events ?? []).map((event) => event.seq ?? 0)) + 1;
            frame.events = [...(frame.events ?? []), { id: "cached-prefetch-marker", seq, at: Date.now(), kind: "assistant", text: marker }];
          }
          if (held) pending.push(JSON.stringify(frame));
          else ws.send(JSON.stringify(frame));
          if (connection === 2 && frame.t === "synced") secondCaptured = true;
        }
      });
      ws.onMessage((message) => {
        const frame = JSON.parse(String(message)) as { t?: string };
        if (frame.t === "cmd") commands.push(frame);
        server.send(message);
      });
    },
  );
  await page.goto("/agents");
  const open = page.locator('.ms-row[data-agent="tree-lead"] .ms-go');
  await expect(open).toBeVisible({ timeout: 30_000 });
  await page.clock.pauseAt(new Date((await page.evaluate(() => Date.now())) + 60_000));
  await open.focus();
  await page.clock.runFor(60);
  await expect.poll(() => firstClosed).toBe(true);
  expect(commands).toEqual([]);
  await open.click();
  await page.clock.resume();
  await expect.poll(() => secondCaptured).toBe(true);
  await expect(page.getByText(marker, { exact: true })).toBeVisible();
  const field = page.getByRole("textbox", { name: "Message to the agent", exact: true });
  await expect(field).toBeEnabled();
  await expect(field).toHaveValue(draft);
  await expect(field).toHaveAttribute("placeholder", /^Not connected:/);
  await expect(page.locator(".hx-dock .cp-mic")).toBeDisabled();
  const connection = page.locator(".ag-turn-control");
  await expect(connection).toHaveAccessibleName("Reconnecting…");
  await expect(connection).toHaveAttribute("data-state", "disconnected");
  await expect(connection).toBeDisabled();
  await field.fill(draft + " Still here.");
  expect(connections).toBe(2);
  expect(commands).toEqual([]);
  release();
  await expect(field).not.toHaveAttribute("placeholder", /^Not connected:/);
  await expect(field).toHaveValue(draft + " Still here.");
  await expect(page.getByText(marker, { exact: true })).toBeVisible();
  expect(connections).toBe(2);
  expect(errors).toEqual([]);
});
