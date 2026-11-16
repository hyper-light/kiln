/**
 * Mission control reads its agents lean (docs/ci.md W21-R02; docs/agents.md, "The snapshot, lean"): the list
 * without each agent's last words, which only the rows on the page show. In the production build against the
 * mock, which answers lean as the server does: the list opens with its rows' lines, as it does read whole, their
 * last words having come with the snapshot; a row scrolled to has its line by the time it shows, asked for with
 * the rest of its window in one request; and rows whose last words the snapshot didn't carry wait for them.
 */
import { expect, test, type Page } from "@playwright/test";

test.skip(!!process.env.E2E_BASE_URL, "the mock's agents");

const LIVE = 'ul[data-list="live"]';
type Line = { id: string; stand: string; text: string; shows: boolean };
type Frame = { skeleton: boolean; rows: Line[] };

/** Each row of the live list in the page: its agent, how it stands, its line, and whether any of it is in the viewport. */
const lines = (page: Page) =>
  page.locator(`${LIVE} > li.ms-row`).evaluateAll((rows) =>
    rows.map((r) => {
      const b = r.getBoundingClientRect();
      return { id: r.getAttribute("data-agent") ?? "", stand: r.getAttribute("data-stand") ?? "", text: r.querySelector(".ms-text")?.textContent ?? "", shows: b.bottom > 0 && b.top < innerHeight };
    }),
  );
/** Every frame's rows from the page's first, kept by the page itself. */
async function record(page: Page) {
  await page.addInitScript((list: string) => {
    const w = window as unknown as { lwFrames: Frame[] };
    w.lwFrames = [];
    const step = () => {
      const rows = [...document.querySelectorAll(`${list} > li.ms-row`)].map((r) => {
        const b = r.getBoundingClientRect();
        return { id: r.getAttribute("data-agent") ?? "", stand: r.getAttribute("data-stand") ?? "", text: r.querySelector(".ms-text")?.textContent ?? "", shows: b.bottom > 0 && b.top < innerHeight };
      });
      w.lwFrames.push({ skeleton: !!document.querySelector(".ms-loading"), rows });
      requestAnimationFrame(step);
    };
    requestAnimationFrame(step);
  }, LIVE);
}
const frames = (page: Page) => page.evaluate(() => (window as unknown as { lwFrames: Frame[] }).lwFrames);
/** The page's reads of the agents' list (and of their last words) and of the attention snapshot, as it asks them. */
function readsOf(page: Page): URL[] {
  const asked: URL[] = [];
  page.on("request", (r) => {
    const url = new URL(r.url());
    if (r.method() === "GET" && (url.pathname === "/api/harness/sessions" || url.pathname === "/api/harness/attention")) asked.push(url);
  });
  return asked;
}
const summaries = (asked: URL[]) => asked.filter((u) => u.searchParams.has("summaries")).map((u) => (u.searchParams.get("summaries") ?? "").split(","));
/** What each agent last said, as the list tells it whole. */
async function words(page: Page): Promise<Map<string, string>> {
  const whole = (await (await page.request.get("/api/harness/sessions")).json()) as { id: string; summary: string }[];
  return new Map(whole.map((a) => [a.id, a.summary]));
}
/** The rows that show what their agent last said (finished ones) whose agent has said something. */
const saying = (rows: Line[], said: Map<string, string>) => rows.filter((r) => r.stand === "finished" && said.get(r.id));
async function open(page: Page) {
  await page.goto("/agents");
  await page.locator(`${LIVE} > li.ms-row`).first().waitFor();
  // What loads after the rows show has come in: the list where it was at the last look.
  let last = "";
  await expect
    .poll(async () => {
      const now = await page.evaluate((list) => `${(document.querySelector(list)?.getBoundingClientRect().top ?? 0) + scrollY} ${document.documentElement.scrollHeight}`, LIVE);
      const same = now === last;
      last = now;
      return same;
    })
    .toBe(true);
}

test.beforeEach(async ({ page }) => {
  expect((await page.request.post("/api/mock/attention/reset")).ok()).toBe(true);
});

test("W21-R02 the list opens with its rows' lines as it does read whole: the agents lean, their last words with the snapshot, none asked for after", async ({ page }) => {
  // A tall window: past the rows that ask for you, which come first and show their requests, to rows that show their agents' last words.
  const size = page.viewportSize() ?? { width: 1440, height: 900 };
  await page.setViewportSize({ width: size.width, height: 1600 });
  const asked = readsOf(page);
  await record(page);
  await open(page);
  const lean = await lines(page);
  const said = await words(page);
  expect(saying(lean, said).length, "rows that show what their agent last said").toBeGreaterThan(0);

  // One read of the list, lean; the snapshot lean, with the last words of as many rows as the page can open on; none of them asked for.
  expect(asked.filter((u) => u.pathname === "/api/harness/sessions").map((u) => u.search)).toEqual(["?lean=1"]);
  const snapshots = asked.filter((u) => u.pathname === "/api/harness/attention");
  expect(snapshots.length).toBeGreaterThan(0);
  const rows = await page.evaluate(() => Math.ceil((2 * innerHeight) / 84));
  for (const u of snapshots) expect([u.searchParams.get("lean"), u.searchParams.get("said")]).toEqual(["1", String(rows)]);

  // From its first frame with rows, each such row had its line: none was drawn without it, and none changed.
  const seen = await frames(page);
  const first = new Map<string, string>();
  for (const frame of seen) {
    for (const r of saying(frame.rows, said)) {
      expect(r.text, `${r.id}'s line`).not.toBe("");
      if (!first.has(r.id)) first.set(r.id, r.text);
      expect(r.text, `${r.id}'s line stays what it was first drawn as`).toBe(first.get(r.id));
    }
  }

  // The same page reading the list and the snapshot whole, as before they were lean: the same rows, each with the same line.
  const whole = await page.context().newPage();
  const list = await (await page.request.get("/api/harness/sessions")).text();
  await whole.route(
    (url) => url.pathname === "/api/harness/sessions" && url.searchParams.get("lean") === "1",
    (route) => route.fulfill({ status: 200, contentType: "application/json", body: list }),
  );
  await whole.route(
    (url) => url.pathname === "/api/harness/attention" && url.searchParams.has("lean"),
    (route) => {
      const url = new URL(route.request().url());
      url.searchParams.delete("lean");
      url.searchParams.delete("said");
      return route.continue({ url: url.toString() });
    },
  );
  await whole.setViewportSize({ width: size.width, height: 1600 });
  await open(whole);
  const before = new Map((await lines(whole)).map((r) => [r.id, r]));
  // (The mock's agents work on between the two looks: the rows finished at both are the ones compared.)
  const both = lean.filter((r) => r.stand === "finished" && before.get(r.id)?.stand === "finished");
  expect(saying(both, said).length, "rows that show what their agent last said, in both").toBeGreaterThan(0);
  expect(both.map((r) => `${r.id}: ${r.text}`)).toEqual(both.map((r) => `${r.id}: ${before.get(r.id)?.text}`));
  await whole.close();
});

test("W21-R02 a row scrolled to has its line by the time it shows, asked for with the rest of its window in one request", async ({ page }) => {
  // A short window: the list is longer than the rows it opens on.
  const size = page.viewportSize() ?? { width: 1440, height: 900 };
  await page.setViewportSize({ width: size.width, height: 500 });
  const asked = readsOf(page);
  await record(page);
  await open(page);
  const said = await words(page);
  expect(summaries(asked), "the rows it opens on have theirs from the snapshot").toEqual([]);
  // Down the list, half a viewport at a time, as someone reading it.
  let steps = 0;
  for (;;) {
    const moved = await page.evaluate(() => {
      const was = scrollY;
      scrollBy(0, innerHeight / 2);
      return scrollY !== was;
    });
    if (!moved) break;
    steps++;
    await page.evaluate(() => new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(done))));
  }
  // Every row of the list that says what its agent last said has its line.
  await expect.poll(async () => saying(await lines(page), said).filter((r) => r.text === "").length).toBe(0);
  const each = summaries(asked);
  expect(each.length, "rows past those it opened on asked for theirs").toBeGreaterThan(0);
  expect(each.length, "a request for each window scrolled to at most, not one a row").toBeLessThanOrEqual(steps);
  const ids = each.flat();
  expect(new Set(ids).size, "no agent's asked for twice").toBe(ids.length);
  // And at no frame did a row in view stand without its line: theirs were asked for before they showed.
  for (const frame of await frames(page)) for (const r of saying(frame.rows, said)) if (r.shows) expect(r.text, `${r.id}'s line as it showed`).not.toBe("");
});

test("W21-R02 the list's last row, kept in the page for Tab to end on, has its line from the snapshot too", async ({ page }) => {
  // The mock's order is each agent's last use, then its id: with the one whose id comes last opened, the list's
  // last row is a finished one. A short window, so that row is far past the rows the list opens on.
  expect((await page.request.post("/api/harness/attention", { data: { action: "visit", agent: "tree-lead", source: "direct", key: "last-row", mode: "refresh" } })).ok()).toBe(true);
  const size = page.viewportSize() ?? { width: 1440, height: 900 };
  await page.setViewportSize({ width: size.width, height: 500 });
  const asked = readsOf(page);
  await open(page);
  const said = await words(page);
  const rows = await lines(page);
  const last = rows.at(-1);
  expect(last && saying([last], said).length, "the last row in the page shows what its agent last said").toBe(1);
  expect(last?.shows, "and it's far below what shows").toBe(false);
  expect(last?.text).not.toBe("");
  expect(await page.locator(`${LIVE} > li[data-gap]`).count(), "rows between the first ones and it aren't in the page").toBeGreaterThan(0);
  expect(summaries(asked), "its last words came with the snapshot, as the first rows' did").toEqual([]);
});

test("W21-R02 rows whose last words the snapshot didn't carry wait for them: the list still opens with its lines", async ({ page }) => {
  // As where the list is kept in another order than the snapshot's (another sort, a filter): here the snapshot is
  // asked for none, so none of the rows the list opens on has its last words from it. And their answer comes late.
  const size = page.viewportSize() ?? { width: 1440, height: 900 };
  await page.setViewportSize({ width: size.width, height: 1600 });
  await page.route(
    (url) => url.pathname === "/api/harness/attention" && url.searchParams.has("said"),
    (route) => {
      const url = new URL(route.request().url());
      url.searchParams.delete("said");
      return route.continue({ url: url.toString() });
    },
  );
  await page.route(
    (url) => url.pathname === "/api/harness/sessions" && url.searchParams.has("summaries"),
    async (route) => {
      // Long enough to draw rows without their lines, were they drawn before it.
      await new Promise((r) => setTimeout(r, 300));
      await route.continue();
    },
  );
  const asked = readsOf(page);
  await record(page);
  await open(page);
  const said = await words(page);
  const now = await lines(page);
  expect(saying(now, said).length, "rows that show what their agent last said").toBeGreaterThan(0);
  expect(summaries(asked).length, "its first rows' last words: asked for, in one request").toBe(1);
  // No frame drew one of the rows it opened on without its line, though its answer came 300 ms late.
  const opened = new Set(now.map((r) => r.id));
  for (const frame of await frames(page)) for (const r of saying(frame.rows, said)) if (opened.has(r.id)) expect(r.text, `${r.id}'s line`).not.toBe("");
});
