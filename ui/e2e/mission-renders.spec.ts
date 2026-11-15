/**
 * A row in Mission control is drawn again only when what it says changes (docs/ci.md W26-R15): not
 * at each word from another agent, whether or not it's pinned or its agent has subagents kiln
 * started for it; and of an open tree's subagents, none for news that isn't theirs. Counted, not
 * timed: which rows each of the page's renders drew again (as React's own tools read it), beside
 * the news the server sent of each agent. In the production build against the mock.
 */
import { expect, test, type Page } from "@playwright/test";
import { firstNotWaiting } from "./mission-list";

const LIVE = 'ul[data-list="live"]';

test.skip(!!process.env.E2E_BASE_URL, "the mock's agents");

/** How many times each row (by its agent) and each subagent's row in a tree (by its key) was drawn again: not counting the first time it's put in the page. */
type Drawn = { rows: Record<string, number>; nodes: Record<string, number> };

/**
 * Counts the page's renders from before its code runs. React tells its DevTools' hook of every
 * commit, in a production build too; a component a commit worked on that was there before it, and
 * did the work again (its fiber's first flag, PerformedWork), was drawn again. A row is known by
 * what it's given: its agent and the list's actions (mission.tsx `Row`), or its tree's row and
 * the tree's remove (`MissionNode`).
 */
async function count(page: Page) {
  await page.addInitScript(() => {
    type Fiber = { tag: number; flags: number; memoizedProps: Record<string, unknown> | null; alternate: Fiber | null; child: Fiber | null; sibling: Fiber | null };
    const drawn: Drawn = { rows: {}, nodes: {} };
    (window as unknown as { kilnDrawn: Drawn }).kilnDrawn = drawn;
    const note = (f: Fiber) => {
      const given = f.memoizedProps;
      if (!given || typeof given !== "object") return;
      const agent = (given.a as { id?: string } | undefined)?.id;
      const node = (given.r as { key?: string } | undefined)?.key;
      if (agent && "onOrganize" in given) drawn.rows[agent] = (drawn.rows[agent] ?? 0) + 1;
      else if (node && "onRemove" in given) drawn.nodes[node] = (drawn.nodes[node] ?? 0) + 1;
    };
    // Function components (plain, forwarding a ref, memoized) the commit went through: one whose children are the ones it had was left alone, and so was all under it.
    const walk = (first: Fiber | null) => {
      for (let f = first; f; f = f.sibling) {
        const was = f.alternate;
        if (!was) continue;
        if ((f.tag === 0 || f.tag === 11 || f.tag === 15) && f.flags & 1) note(f);
        if (f.child !== was.child) walk(f.child);
      }
    };
    (window as unknown as { __REACT_DEVTOOLS_GLOBAL_HOOK__: unknown }).__REACT_DEVTOOLS_GLOBAL_HOOK__ = {
      supportsFiber: true,
      isDisabled: false,
      renderers: new Map(),
      inject: () => 1,
      checkDCE: () => undefined,
      onCommitFiberUnmount: () => undefined,
      onPostCommitFiberRoot: () => undefined,
      onCommitFiberRoot: (_renderer: number, root: { current: Fiber }) => {
        const now = root.current;
        if (now.alternate && now.child !== now.alternate.child) walk(now.child);
      },
    };
  });
  return {
    drawn: () => page.evaluate(() => (window as unknown as { kilnDrawn: Drawn }).kilnDrawn),
    /** From here on. */
    again: () =>
      page.evaluate(() => {
        const drawn = (window as unknown as { kilnDrawn: Drawn }).kilnDrawn;
        drawn.rows = {};
        drawn.nodes = {};
      }),
  };
}

/** Each agent's news as the server sends it (`/api/events`): how many times what it says of the agent changed, from `from()` on. */
function listen(page: Page) {
  const news: Record<string, number> = {};
  const said: Record<string, string> = {};
  let on = false;
  page.on("websocket", (ws) =>
    ws.on("framereceived", ({ payload }) => {
      if (typeof payload !== "string" || !payload.startsWith("{")) return;
      let event: { type?: string; info?: { id?: string } };
      try {
        event = JSON.parse(payload) as typeof event;
      } catch {
        return;
      }
      if (event.type !== "harness" || !event.info?.id) return;
      const now = JSON.stringify(event.info);
      if (on && said[event.info.id] !== now) news[event.info.id] = (news[event.info.id] ?? 0) + 1;
      said[event.info.id] = now;
    }),
  );
  return {
    news,
    from: () => {
      on = true;
    },
  };
}

const row = (page: Page, id: string) => page.locator(`${LIVE} > li.ms-row[data-agent="${id}"]`);
const inPage = (page: Page) => page.locator(`${LIVE} > li.ms-row`).evaluateAll((rows) => rows.map((r) => r.getAttribute("data-agent") ?? ""));
/** What the mock says agent `id` is doing now. */
async function doing(page: Page, id: string): Promise<string> {
  const all = (await (await page.request.get("/api/harness/sessions")).json()) as { id: string; doing?: string }[];
  return all.find((a) => a.id === id)?.doing ?? "";
}
/** News of agent `id`, `times` times, each taken in by the page before the next is sent: what it's doing, and nothing else of it. */
async function tell(page: Page, id: string, times: number, drawn: () => Promise<Drawn>) {
  for (let n = 1; n <= times; n++) {
    expect((await page.request.post("/api/mock/agent-patch", { data: { id, patch: { doing: `Checking the figures (${n})` } } })).ok()).toBe(true);
    await expect.poll(async () => (await drawn()).rows[id] ?? 0, { message: `${id}'s row, drawn again once for each of its ${n} news` }).toBe(n);
  }
}

test("W26-R15 news of one agent draws its row again, once, and no other: not a pinned row, not one whose agent has subagents of kiln's", async ({ page }) => {
  expect((await page.request.post("/api/mock/attention/reset")).ok()).toBe(true);
  await page.request.post("/api/mock/asks");
  const renders = await count(page);
  const heard = listen(page);
  await page.goto("/agents");
  await page.locator(`${LIVE} > li.ms-row`).first().waitFor();
  // The first agent at work that isn't waiting on you: the one with news. The one after it, pinned from its menu.
  const told = await firstNotWaiting(page, '[data-stand="working"]');
  const pinned = await row(page, told).evaluate((r) => {
    for (let next = r.nextElementSibling; next; next = next.nextElementSibling) if (next.matches("li.ms-row")) return next.getAttribute("data-agent") ?? "";
    return "";
  });
  expect(pinned).not.toBe("");
  await row(page, pinned)
    .getByRole("button", { name: /^Actions for / })
    .click();
  await page.getByRole("menuitem", { name: "Pin", exact: true }).click();
  await expect(row(page, pinned).getByRole("img", { name: "Pinned" })).toBeVisible();
  // "Gate order" waits on you for a subagent kiln started for it: its row is made of the two of them.
  const family = "ask-lead";
  await expect(row(page, family)).toHaveCount(1);
  const was = await doing(page, told);
  try {
    // Its menu closed, the focus is back on the pinned row's button, and the list holds still while it's there: taken off it, and the list left alone until it follows its agents again.
    await expect(row(page, pinned).getByRole("button", { name: /^Actions for / })).toBeFocused();
    await page.evaluate(() => (document.activeElement as HTMLElement | null)?.blur());
    await expect(page.locator("[data-held]")).toHaveCount(0);
    const before = await inPage(page);
    await renders.again();
    heard.from();
    // Twice: once wouldn't tell a row drawn once for each of its news from one drawn once and never again.
    const times = 2;
    await tell(page, told, times, renders.drawn);
    const drawn = (await renders.drawn()).rows;
    const after = await inPage(page);
    // Of the rows in the page all along, those whose agents had no news: none was drawn again.
    const quiet = before.filter((id) => after.includes(id) && !heard.news[id]);
    expect(quiet).toEqual(expect.arrayContaining([pinned, family]));
    expect(Object.fromEntries(quiet.map((id) => [id, drawn[id] ?? 0]))).toEqual(Object.fromEntries(quiet.map((id) => [id, 0])));
    expect(drawn[told]).toBe(times);
  } finally {
    await page.request.post("/api/mock/agent-patch", { data: { id: told, patch: { doing: was } } });
    await page.request.post("/api/mock/attention/reset");
  }
});

test("W26-R15 with an agent's tree open, news of the agent draws none of its subagents' rows again", async ({ page }) => {
  expect((await page.request.post("/api/mock/attention/reset")).ok()).toBe(true);
  await page.request.post("/api/mock/asks");
  const renders = await count(page);
  await page.goto("/agents");
  await page.locator(`${LIVE} > li.ms-row`).first().waitFor();
  // "Storage lead": subagents kiln started for it, theirs, and its harness's own.
  const lead = "tree-lead";
  // Down the list, as someone looking for it would go, until its row is in the page.
  for (;;) {
    if ((await row(page, lead).count()) > 0) break;
    expect(await page.evaluate(() => (scrollBy(0, innerHeight / 2), scrollY + innerHeight < document.documentElement.scrollHeight)), "Storage lead isn't in the list").toBe(true);
  }
  await row(page, lead).getByRole("button", { name: "Show Storage lead's subagents" }).click();
  const nodes = page.getByRole("list", { name: "Storage lead's subagents" }).locator(".ms-node");
  await expect(nodes.first()).toBeVisible();
  const subagents = await nodes.count();
  expect(subagents).toBeGreaterThan(1);
  const was = await doing(page, lead);
  try {
    await expect(page.locator("[data-held]")).toHaveCount(0);
    await renders.again();
    // Twice, as above.
    const times = 2;
    await tell(page, lead, times, renders.drawn);
    const drawn = await renders.drawn();
    expect(drawn.rows[lead]).toBe(times);
    expect(drawn.nodes).toEqual({});
    expect(await nodes.count()).toBe(subagents);
  } finally {
    await page.request.post("/api/mock/agent-patch", { data: { id: lead, patch: { doing: was } } });
  }
});
