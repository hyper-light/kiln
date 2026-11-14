/**
 * W21-R02: terminals attach in turn, the one in view first (ui/src/views/terminal/attach-order.ts). Every tab's view
 * attaches to its session, and a session's replay is up to 256 KiB: attached all at once, the tab in view has its replay
 * behind the others' on a slow link. So the one in view attaches at once, and each of the others once the one before it has
 * its replay drawn; a tab picked while it waits attaches then.
 */
import { expect, test, type Page } from "@playwright/test";

const SESSIONS = [
  { id: "ts-builds", name: "builds" },
  { id: "ts-logs", name: "logs" },
  { id: "ts-deploys", name: "deploys" },
];

const info = (s: (typeof SESSIONS)[number]) => ({
  id: s.id,
  target: "local",
  command: null,
  title: "",
  name: s.name,
  created_at: 1_794_000_000,
  attached: 0,
  cols: 80,
  rows: 24,
  idle_seconds: 0,
  exited: false,
  exit_code: null,
  error: null,
});

/**
 * The terminal page with three sessions listed, their sockets played here as the server plays them: the `session` frame
 * as each opens, its replay when the test says. `opened` is the sessions whose sockets have opened, in that order.
 */
async function terminals(page: Page) {
  const opened: string[] = [];
  const down = new Map<string, (data: Buffer) => void>();
  await page.route("**/api/terminal/sessions", (route) => route.fulfill({ json: SESSIONS.map(info) }));
  await page.routeWebSocket(
    (url) => url.pathname === "/api/terminal/ws",
    (ws) => {
      const id = new URL(ws.url()).searchParams.get("session") ?? "";
      const session = SESSIONS.find((s) => s.id === id);
      if (!session) return ws.close();
      opened.push(id);
      down.set(id, (data) => ws.send(data));
      ws.send(JSON.stringify({ type: "session", session: { ...info(session), attached: 1 } }));
    },
  );
  await page.goto("/terminal");
  return {
    opened: () => [...opened],
    replay: (id: string) => {
      const send = down.get(id);
      if (!send) throw new Error(`${id}'s socket has not opened`);
      send(Buffer.from(`\x1b[0m$ in ${id} `));
    },
  };
}

const painted = (page: Page) => page.evaluate(() => new Promise<void>((resolve) => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));

test.beforeEach(() => test.skip(!!process.env.E2E_BASE_URL, "controlled terminals' sockets"));

test("W21-R02 the terminal in view attaches first, and each of the others once the one before it has its replay", async ({ page }) => {
  const t = await terminals(page);
  // The first tab is the one in view: its socket opens, and no other's while its replay is on its way.
  await expect.poll(() => t.opened(), { timeout: 30_000 }).toEqual(["ts-builds"]);
  await painted(page);
  await painted(page);
  expect(t.opened()).toEqual(["ts-builds"]);
  t.replay("ts-builds");
  await expect(page.locator(".term-host:not([aria-hidden='true']) .xterm-rows")).toContainText("$ in ts-builds");
  await expect.poll(() => t.opened()).toEqual(["ts-builds", "ts-logs"]);
  await painted(page);
  await painted(page);
  expect(t.opened()).toEqual(["ts-builds", "ts-logs"]);
  t.replay("ts-logs");
  await expect.poll(() => t.opened()).toEqual(["ts-builds", "ts-logs", "ts-deploys"]);
});

test("W21-R02 a terminal picked while it waits its turn attaches then", async ({ page, isMobile }) => {
  const t = await terminals(page);
  await expect.poll(() => t.opened(), { timeout: 30_000 }).toEqual(["ts-builds"]);
  // The last session, picked while the first is still attaching (by its tab, or on a phone from the sessions' list): it
  // doesn't wait for it, nor for the one before it.
  if (isMobile) {
    await page.getByRole("button", { name: "Switch session" }).click();
    await page.getByLabel("Sessions").getByRole("button", { name: "deploys" }).first().click();
  } else await page.getByRole("tab", { name: "deploys" }).click();
  await expect.poll(() => t.opened()).toEqual(["ts-builds", "ts-deploys"]);
  t.replay("ts-deploys");
  await expect(page.locator(".term-host:not([aria-hidden='true']) .xterm-rows")).toContainText("$ in ts-deploys");
  // The one left goes once both before it are done.
  await painted(page);
  await painted(page);
  expect(t.opened()).toEqual(["ts-builds", "ts-deploys"]);
  t.replay("ts-builds");
  await expect.poll(() => t.opened()).toEqual(["ts-builds", "ts-deploys", "ts-logs"]);
});
