/**
 * Ship takes each change in place (W21-R15), against the mock: with the server's `ship` frames on
 * (`POST /api/mock/ship/events?on=1`), a commit pushed reaches kiln's row and its opened commits
 * without the list loading again, and while the mock's CI is busy the page asks for its lists only
 * on its poll; with them off (a server from before them), it loads its lists again on CI's events,
 * as it did. Each mode's requests and bytes over the same window are printed.
 */
import { expect, test, type Page } from "@playwright/test";

const REAL = !!process.env.E2E_BASE_URL;
test.skip(REAL, "drives the mock's commits");

/** What the page asked of Ship's endpoints and what came down its events socket, while counting. */
interface Count {
  /** Its lists (`GET /api/ship/commits?…`). */
  requests: number;
  bytes: number;
  /** A commit's detail (`GET /api/ship/commits/:repo/:sha`), and the rest of Ship's. */
  others: number;
  otherBytes: number;
  /** Every frame of the events socket, and of them the `ship` frames. */
  frames: number;
  frameBytes: number;
  ship: number;
  shipBytes: number;
}

function counter(page: Page) {
  const zero = (): Count => ({ requests: 0, bytes: 0, others: 0, otherBytes: 0, frames: 0, frameBytes: 0, ship: 0, shipBytes: 0 });
  const c: Count = zero();
  let on = false;
  page.on("response", (r) => {
    const path = new URL(r.url()).pathname;
    if (!on || !path.startsWith("/api/ship/")) return;
    const list = path === "/api/ship/commits";
    if (list) c.requests++;
    else c.others++;
    void r
      .body()
      .then((b) => (list ? (c.bytes += b.length) : (c.otherBytes += b.length)))
      .catch(() => undefined);
  });
  page.on("websocket", (ws) => {
    if (!ws.url().includes("/api/events")) return;
    ws.on("framereceived", (f) => {
      if (!on) return;
      const text = typeof f.payload === "string" ? f.payload : f.payload.toString("utf8");
      c.frames++;
      c.frameBytes += text.length;
      if (text.startsWith('{"type":"ship"')) {
        c.ship++;
        c.shipBytes += text.length;
      }
    });
  });
  return {
    count: c,
    start: () => {
      Object.assign(c, zero());
      on = true;
    },
    stop: () => {
      on = false;
      return { ...c };
    },
  };
}

/** Kiln's row opened: its commits, Jobs and Lanes read their resources too. */
async function opened(page: Page) {
  await page.goto("/ship");
  const row = page.locator(".ship-row").filter({ hasText: "hyper-light/kiln" }).first();
  await expect(row).toBeVisible({ timeout: 15_000 });
  await row.locator(".ship-go").click();
  await expect(row.locator(".ship-sech > h3").first()).toBeVisible({ timeout: 15_000 });
  return row;
}

const WINDOW_MS = 10_000;

test("w21_r15 with ship frames, a pushed commit reaches the row in place and busy CI loads no list; without, CI's events load them", async ({ page, isMobile }) => {
  test.skip(isMobile, "requests counted once, at the desktop's width");
  test.setTimeout(120_000);
  await page.request.post("/api/mock/ship/arrive?clear=1");
  const seen = counter(page);
  try {
    // Before: a server that sends no frames.
    await page.request.post("/api/mock/ship/events?on=0");
    await opened(page);
    seen.start();
    await page.waitForTimeout(WINDOW_MS);
    // A loaded host's window may hold fewer of CI's events: what's checked is that they load the lists.
    await expect.poll(() => seen.count.requests, { message: "without frames, CI's events load the lists again", timeout: 30_000 }).toBeGreaterThan(2);
    const before = seen.stop();
    // After: the server's frames.
    await page.request.post("/api/mock/ship/events?on=1");
    await opened(page);
    seen.start();
    await page.waitForTimeout(WINDOW_MS);
    const after = seen.stop();
    console.log(`ship over ${WINDOW_MS / 1000} s, CI busy: before ${JSON.stringify(before)}; after ${JSON.stringify(after)}`);
    // The poll (15 s) may fall in the window, once for each list shown.
    expect(after.requests, "with frames, no list loads on CI's events").toBeLessThanOrEqual(3);
    // A commit pushed: kiln's row (closed, so it doesn't hold its commit for you) shows it, without a load.
    await page.goto("/ship");
    const row = page.locator(".ship-row").filter({ hasText: "hyper-light/kiln" }).first();
    await expect(row).toBeVisible({ timeout: 15_000 });
    seen.start();
    await page.request.post("/api/mock/ship/arrive");
    await expect(row).toContainText("arrived 1", { timeout: 5_000 });
    const pushed = seen.stop();
    expect(pushed.ship, "the push came as a frame").toBeGreaterThan(0);
    expect(pushed.requests, "nothing loaded for it").toBeLessThanOrEqual(1);
  } finally {
    await page.request.post("/api/mock/ship/events?on=0");
    await page.request.post("/api/mock/ship/arrive?clear=1");
  }
});
