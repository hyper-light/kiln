/** W20: owner lifecycle remains usable when the detailed snapshot still says disconnected. */
import { gunzipSync } from "node:zlib";
import { expect, test } from "@playwright/test";

test("owner lifecycle keeps the composer available while details arrive", async ({ page }, info) => {
  test.skip(!!process.env.E2E_BASE_URL, "controlled mock lifecycle frames");
  let staleDetails = 0;
  await page.routeWebSocket(
    (url) => url.pathname === "/api/harness/sessions/tree-lead/ws",
    (socket) => {
      const server = socket.connectToServer();
      server.onMessage((message) => {
        const text = typeof message === "string" ? message : gunzipSync(message).toString("utf8");
        for (const line of text.split("\n").filter(Boolean)) {
          const frame = JSON.parse(line);
          if ((frame.t === "open" || frame.t === "resumed") && frame.info) frame.info.status = "running";
          if (frame.t === "state" && frame.state) {
            frame.state.status = "disconnected";
            staleDetails++;
          }
          socket.send(JSON.stringify(frame));
        }
      });
      socket.onMessage((message) => server.send(message));
    },
  );
  await page.goto("/agents/tree-lead");
  const field = page.getByRole("textbox", { name: "Message to the agent", exact: true });
  await expect(field).toBeVisible();
  await expect(field).toBeEnabled();
  expect(staleDetails).toBeGreaterThan(0);
  await expect(field).not.toHaveAttribute("placeholder", /offline/i);
  await field.fill("Keep this draft while the owner remains online.");
  await expect(field).toHaveValue("Keep this draft while the owner remains online.");
  await expect(page.locator(".ag-title")).toHaveText("Storage lead");
  const width = await page.evaluate(() => ({ scroll: document.documentElement.scrollWidth, client: document.documentElement.clientWidth }));
  expect(width.scroll).toBeLessThanOrEqual(width.client);
  await page.evaluate(() => new Promise<void>((resolve) => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
  await page.screenshot({ path: info.outputPath(`availability-${info.project.name}.png`) });
});
