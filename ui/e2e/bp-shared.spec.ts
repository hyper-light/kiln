/**
 * bp-g3 step 6 (plan 01a1a832eb2f711f9c6f51d88effe89d): one protocol socket per browser, shared by its
 * tabs, in each browser and width (`playwright.live.config.ts`), in both modes (a SharedWorker, and the
 * Web Lock fallback forced). Three tabs of one browser context subscribe to one topic: the server holds
 * exactly one socket for them; each change reaches every tab, the same and once; when the owner goes
 * (its tab closed, or its worker ended as Android ends one) the socket is taken over within 2 s and
 * every subscription resumed from its cursor (no snapshot again).
 *
 * The changes are the server's own: a terminal opened makes the hub say `terminals` changed, which the
 * `changed` topic (kind 7, the owner's) counts.
 */
import { expect, test, type BrowserContext, type Page } from "@playwright/test";

const ORIGIN = process.env.LIVE_ORIGIN ?? "";
const TOKEN = process.env.LIVE_TOKEN ?? "";
/** The `changed` topic, the owner's scope (`crate::bp::kind::CHANGED`). */
const CHANGED = 7;

type Read = { items: Record<string, string>; cursor: { born: number; rev: number } | null; applied: number; snapshots: number; version: number } | null;
type Lab = {
  open: (kind: number, key: string) => number;
  read: (id: number) => Read;
  conn: () => { conn: string; owner: string; owns: boolean; sockets: number; errors: string[] };
  kill: () => void;
};

const lab = (page: Page) => page.evaluate.bind(page);

async function sessions(context: BrowserContext): Promise<{ id: number; session: string; opened_ms: number }[]> {
  const r = await context.request.get(`${ORIGIN}/api/live/sockets`);
  expect(r.ok()).toBeTruthy();
  return ((await r.json()) as { sockets: { id: number; session: string; opened_ms: number }[] }).sockets;
}

/** Something the hub says changed: a terminal opened (and closed). */
async function change(context: BrowserContext): Promise<void> {
  const r = await context.request.post(`${ORIGIN}/api/terminal/sessions`, { data: { target: "local", cmd: "true" } });
  expect(r.ok(), await r.text()).toBeTruthy();
}

async function tab(context: BrowserContext, mode: string): Promise<{ page: Page; id: number }> {
  const page = await context.newPage();
  await page.goto(`${ORIGIN}/__live/lab.html?mode=${mode}`);
  await page.waitForSelector("body[data-ready='1']", { state: "attached" });
  const id = await lab(page)((k: number) => (globalThis as unknown as { lab: Lab }).lab.open(k, ""), CHANGED);
  await expect
    .poll(() => lab(page)((i: number) => (globalThis as unknown as { lab: Lab }).lab.read(i)?.cursor ?? null, id), {
      timeout: 20_000,
      message: `subscribed (${mode}): ${JSON.stringify(await conn(page).catch(() => null))}`,
    })
    .not.toBeNull();
  return { page, id };
}

const read = (t: { page: Page; id: number }) => lab(t.page)((i: number) => (globalThis as unknown as { lab: Lab }).lab.read(i), t.id);
const conn = (page: Page) => lab(page)(() => (globalThis as unknown as { lab: Lab }).lab.conn());

for (const mode of ["worker", "lock"] as const) {
  test(`bp_g3_s6 three tabs share one socket, take over within 2 s and resume (${mode})`, async ({ context }) => {
    test.skip(!ORIGIN, "needs a lab server: LIVE_ORIGIN and LIVE_TOKEN");
    const login = await context.request.post(`${ORIGIN}/api/auth/login`, { data: { token: TOKEN } });
    expect(login.ok()).toBeTruthy();
    const before = new Set((await sessions(context)).map((s) => s.id));
    const tabs = [await tab(context, mode), await tab(context, mode), await tab(context, mode)];
    const mine = () => sessions(context).then((all) => all.filter((s) => !before.has(s.id)));
    await expect.poll(async () => (await mine()).length, { timeout: 10_000 }).toBe(1);

    // Every change reaches every tab, the same.
    for (let i = 0; i < 3; i++) await change(context);
    await expect.poll(async () => new Set((await Promise.all(tabs.map(read))).map((r) => JSON.stringify([r?.items, r?.cursor]))).size, { timeout: 10_000 }).toBe(1);
    const settled = await Promise.all(tabs.map(read));
    expect(settled[0]?.cursor?.rev ?? 0).toBeGreaterThan(0);
    for (const r of settled) expect(r?.snapshots).toBe(1);
    expect((await mine()).length).toBe(1);

    // The owner goes: a tab closed (the lock's holder), or the worker ended.
    let rest = tabs;
    if (mode === "lock") {
      const owners = await Promise.all(tabs.map((t) => conn(t.page).then((c) => c.owns)));
      expect(owners.filter(Boolean).length).toBe(1);
      const owner = tabs[owners.indexOf(true)]!;
      rest = tabs.filter((t) => t !== owner);
      const versions = await Promise.all(rest.map(read));
      const t0 = Date.now();
      await owner.page.close();
      await change(context);
      await expect.poll(async () => (await Promise.all(rest.map(read))).every((r, i) => (r?.version ?? 0) > (versions[i]?.version ?? 0)), { timeout: 5_000, intervals: [25] }).toBe(true);
      expect(Date.now() - t0).toBeLessThanOrEqual(2_000);
    } else {
      const versions = await Promise.all(rest.map(read));
      const t0 = Date.now();
      await lab(tabs[0]!.page)(() => (globalThis as unknown as { lab: Lab }).lab.kill());
      await change(context);
      await expect.poll(async () => (await Promise.all(rest.map(read))).every((r, i) => (r?.version ?? 0) > (versions[i]?.version ?? 0)), { timeout: 5_000, intervals: [25] }).toBe(true);
      expect(Date.now() - t0).toBeLessThanOrEqual(2_000);
    }
    // Resumed, not reopened: no tab took a snapshot again; they agree; one socket held.
    await expect.poll(async () => new Set((await Promise.all(rest.map(read))).map((r) => JSON.stringify([r?.items, r?.cursor]))).size, { timeout: 10_000 }).toBe(1);
    for (const r of await Promise.all(rest.map(read))) expect(r?.snapshots, "resumed from its cursor").toBe(1);
    await expect.poll(async () => (await mine()).length, { timeout: 10_000 }).toBe(1);
  });
}
