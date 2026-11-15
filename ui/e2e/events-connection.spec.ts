import { expect, test, type WebSocketRoute } from "@playwright/test";

test("W21-R02 the live indicator recovers from a lost stream and stays connected", async ({ page }) => {
  const sockets: WebSocketRoute[] = [];
  await page.routeWebSocket(/\/api\/events(?:\?|$)/, (ws) => {
    sockets.push(ws);
    ws.send(JSON.stringify({ type: "queue", queued: 0, running: 0 }));
  });
  await page.goto("/system");
  const indicator = page.locator(".conn:visible");
  await expect(indicator).toHaveAttribute("data-state", "open");
  expect(sockets).toHaveLength(1);
  await sockets[0]?.close({ code: 1012, reason: "test server restart" });
  await expect.poll(() => sockets.length).toBe(2);
  await expect(indicator).toHaveAttribute("data-state", "open");
  // Tab/network wakeups must not replace a healthy stream after it recovers.
  await page.evaluate(() => {
    window.dispatchEvent(new Event("online"));
    document.dispatchEvent(new Event("visibilitychange"));
  });
  await expect(indicator).toHaveAttribute("aria-label", "Event stream: Live");
  expect(sockets).toHaveLength(2);
});
