/**
 * Mission control's lists are windowed (docs/ci.md W21-R02, src/lib/virtual.ts): only the rows near
 * the viewport are in the page, gaps keeping the room of the rest, so the page is as tall and every
 * row where it would be with all of them in it. In the production build against the mock, at a
 * desktop's and a phone's own heights: what's mounted and what the gaps keep; rows mounted and let
 * go as the page scrolls; the row holding the focus kept, and Tab going on to the next; a row that
 * comes into the window with a new order gliding from where it was, and one that leaves gliding
 * out; what's in view holding still, frame by frame, as what's above it is measured, where the
 * browser anchors the scroll and where it doesn't.
 */
import { expect, test, type Page } from "@playwright/test";
import { firstNotWaiting } from "./mission-list";

test.skip(!!process.env.E2E_BASE_URL, "the mock's agents");

const LIVE = 'ul[data-list="live"]';
const row = (page: Page, id: string) => page.locator(`${LIVE} > li.ms-row[data-agent="${id}"]`);
/** The live list's rows in the page, in order. */
const mounted = (page: Page) => page.locator(`${LIVE} > li.ms-row`).evaluateAll((rows) => rows.map((r) => r.getAttribute("data-agent") ?? ""));
/** Each row in the page: where it is on the page (scrolling doesn't move it) and how tall. */
const boxes = (page: Page) =>
  page.locator(`${LIVE} > li.ms-row`).evaluateAll((rows) =>
    rows.map((r) => {
      const b = r.getBoundingClientRect();
      return { id: r.getAttribute("data-agent") ?? "", top: b.top + scrollY, height: b.height };
    }),
  );
/** The middle of some heights, as the window takes a row it hasn't measured to be (src/lib/virtual.ts `typicalHeight`). */
function middle(heights: number[]): number {
  const sorted = [...heights].sort((a, b) => a - b);
  const mid = sorted.length >> 1;
  return sorted.length % 2 === 1 ? (sorted[mid] ?? 0) : ((sorted[mid - 1] ?? 0) + (sorted[mid] ?? 0)) / 2;
}
/** A window as tall as the page: every row of every list is in it. */
async function wholePage(page: Page) {
  const size = page.viewportSize();
  if (size) await page.setViewportSize({ width: size.width, height: await page.evaluate(() => document.documentElement.scrollHeight) });
  await expect(page.locator(`${LIVE} > li[data-gap]`)).toHaveCount(0);
}
/** Once nothing on the page moves by itself any more (what loads after the rows show has come in): the list where it was at the last look, the page as tall. */
async function settled(page: Page) {
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
/** Whether a gap shows: a row any part of which is in the viewport that isn't in the page. */
const gapShows = (page: Page) =>
  page.locator(`${LIVE} > li[data-gap]`).evaluateAll((gaps) =>
    gaps.some((g) => {
      const b = g.getBoundingClientRect();
      return b.bottom > 0 && b.top < innerHeight;
    }),
  );
/** The rows of the live list more than a viewport over the viewport: none is there but those kept wherever the page goes. */
const farAbove = (page: Page) =>
  page.locator(`${LIVE} > li.ms-row`).evaluateAll((rows) => rows.filter((r) => r.getBoundingClientRect().bottom < -innerHeight).map((r) => r.getAttribute("data-agent") ?? ""));
const toTheFoot = (page: Page) => page.evaluate(() => scrollTo(0, document.documentElement.scrollHeight));

async function open(page: Page) {
  expect((await page.request.post("/api/mock/attention/reset")).ok()).toBe(true);
  await page.request.post("/api/mock/asks");
  await page.goto("/agents");
  await page.locator(`${LIVE} > li.ms-row`).first().waitFor();
  await settled(page);
}

test("W21-R02 only the rows near the viewport are in the page, the page as tall and each of them where it would be with every row in it", async ({ page }) => {
  await open(page);
  const size = page.viewportSize();
  expect(size).not.toBeNull();
  if (!size) return;
  const near = await boxes(page);
  await expect(page.locator(`${LIVE} > li[data-gap]`)).not.toHaveCount(0);
  /** The list's own height (the page under it is as tall as the window at least, whatever the list). */
  const tall = () => page.locator(LIVE).evaluate((ul) => ul.getBoundingClientRect().height);
  const windowed = await tall();
  // The same page with every row in it.
  await wholePage(page);
  const whole = await boxes(page);
  expect(near.length).toBeLessThan(whole.length);
  // Those any part of which is within a viewport of the viewport (the page at its top: down to two viewports), and the last (where Tab ends).
  const last = whole.at(-1);
  expect(near.map((r) => r.id)).toEqual(whole.filter((r) => r.top < 2 * size.height || r === last).map((r) => r.id));
  // Each where it is with every row in the page: the run from the top exactly; the last after a gap that takes each row
  // it stands for to be the middle of those measured (so off by what those rows differ from it, and no more).
  const at = new Map(whole.map((r) => [r.id, r]));
  for (const r of near.slice(0, -1)) expect(Math.abs(r.top - (at.get(r.id)?.top ?? NaN)), `${r.id} moved`).toBeLessThan(0.5);
  const unmounted = whole.filter((r) => !near.some((n) => n.id === r.id));
  const assumed = middle(near.map((r) => r.height));
  const off = unmounted.reduce((sum, r) => sum + (assumed - r.height), 0);
  const end = near.at(-1);
  expect(Math.abs((end?.top ?? NaN) - ((last?.top ?? NaN) + off)), "the last row isn't where its gap puts it").toBeLessThan(0.5);
  expect(Math.abs(windowed - ((await tall()) + off)), "the list's height").toBeLessThan(0.5);
});

test("W21-R02 scrolling mounts the rows it comes to and lets go of those it leaves: every row in its turn, never more than a window's worth", async ({ page }) => {
  await open(page);
  const size = page.viewportSize();
  if (!size) return;
  const first = (await mounted(page))[0] ?? "";
  const seen = new Set<string>();
  let most = 0;
  let shortest = Infinity;
  for (;;) {
    // No gap in what shows: every row any part of which is in the viewport is in the page.
    await expect.poll(() => gapShows(page), { message: "a gap shows" }).toBe(false);
    const now = await boxes(page);
    most = Math.max(most, now.length);
    shortest = Math.min(shortest, ...now.map((r) => r.height));
    now.forEach((r) => seen.add(r.id));
    // A viewport on, as Page Down goes.
    const moved = await page.evaluate(() => {
      const was = scrollY;
      scrollBy(0, innerHeight);
      return scrollY !== was;
    });
    if (!moved) break;
  }
  // Three viewports of rows (the one in view, one over and one under it; one more cut by their ends), and the two Tab starts and ends in.
  expect(most).toBeLessThanOrEqual(Math.ceil((3 * size.height) / shortest) + 1 + 2);
  // At the foot of the page the first row is still there (Tab from above the list lands in it), the ones after it are not.
  await expect.poll(async () => (await mounted(page))[0]).toBe(first);
  expect((await mounted(page)).length).toBeLessThan(seen.size);
  // And back at the top, the rows that were there are there again.
  await page.evaluate(() => scrollTo(0, 0));
  await expect.poll(async () => (await mounted(page)).slice(0, 5)).toEqual([...seen].slice(0, 5));
  // Every row was mounted in its turn: as many as the list has with all of it in the page.
  await wholePage(page);
  expect((await mounted(page)).sort()).toEqual([...seen].sort());
});

test("W21-R02 the row holding the focus stays in the page wherever the page goes, and Tab goes on to the row after it", async ({ page }) => {
  await open(page);
  const near = await mounted(page);
  const [held, next] = [near[2] ?? "", near[3] ?? ""];
  // The last thing in the row that Tab stops at.
  await row(page, held).evaluate((r) =>
    [...r.querySelectorAll<HTMLElement>("button, a[href], input, select, textarea")]
      .filter((el) => el.tabIndex >= 0 && !(el as HTMLButtonElement).disabled && el.getClientRects().length > 0)
      .at(-1)
      ?.focus(),
  );
  await toTheFoot(page);
  // More than a viewport over the viewport now, and still there with the focus in it; so are the rows either side of it
  // (where Tab and Shift+Tab go), and the first row (where Tab comes into the list). Nothing else that far up is.
  await expect.poll(() => farAbove(page)).toEqual([near[0], near[1], held, next]);
  expect(await row(page, held).evaluate((r) => r.contains(document.activeElement))).toBe(true);
  await page.keyboard.press("Tab");
  expect(await row(page, next).evaluate((r) => r.contains(document.activeElement))).toBe(true);
  // The focus gone from the list and the page away again, the rows it kept go too: the first row alone is left that far up.
  await page.evaluate(() => (document.activeElement as HTMLElement | null)?.blur());
  await toTheFoot(page);
  await expect.poll(() => farAbove(page)).toEqual([near[0]]);
});

/** What the page did with the row `id` in the live list, in order: put in (with the animations it had then), a glide begun, that glide over (or cut short), taken out. */
type Trace = { what: "in" | "glide" | "over" | "cut" | "out"; frames: { transform?: string; opacity?: string }[] }[];
async function watch(page: Page, id: string) {
  await page.evaluate(
    ([list, id]) => {
      const trace: Trace = [];
      (window as unknown as { rowTrace: Trace }).rowTrace = trace;
      const ul = document.querySelector(list as string);
      if (!ul) return;
      const framesOf = (el: Element) =>
        el
          .getAnimations()
          .flatMap((a) => (a.effect instanceof KeyframeEffect ? a.effect.getKeyframes().map((k) => ({ transform: k.transform as string | undefined, opacity: k.opacity as string | undefined })) : []));
      const mine = (n: Node): n is HTMLElement => n instanceof HTMLElement && n.getAttribute("data-agent") === id;
      new MutationObserver((records) => {
        for (const r of records) {
          // A row gliding can't be pressed (list-tools.tsx `useGlide`): that's set as its glide begins.
          if (r.type === "attributes" && mine(r.target) && r.target.style.pointerEvents === "none") {
            trace.push({ what: "glide", frames: framesOf(r.target) });
            for (const a of r.target.getAnimations())
              void a.finished.then(
                () => trace.push({ what: "over", frames: [] }),
                () => trace.push({ what: "cut", frames: [] }),
              );
          }
          for (const n of r.addedNodes) if (mine(n)) trace.push({ what: "in", frames: framesOf(n) });
          for (const n of r.removedNodes) if (mine(n) && !n.isConnected) trace.push({ what: "out", frames: [] });
        }
      }).observe(ul, { childList: true, subtree: true, attributes: true, attributeFilter: ["style"] });
    },
    [LIVE, id],
  );
  return () => page.evaluate(() => (window as unknown as { rowTrace: Trace }).rowTrace);
}
/** How far up or down a glide starts from where it ends (px, down positive): its first frame's. */
const startsFrom = (t: Trace[number] | undefined) => Number(/translateY\((-?[\d.]+)px\)/.exec(t?.frames.find((f) => f.transform && f.transform !== "none")?.transform ?? "")?.[1] ?? NaN);

test("W21-R02 a row that comes into the window with a new order glides from where it was, and one that leaves glides out before it's let go", async ({ page }) => {
  await open(page);
  // "Scheduler audit": far down the list.
  const id = "sub-codex";
  const size = page.viewportSize();
  if (!size) return;
  // Where it is in the list, from the rows over it (with every row in the page), to know how far it comes.
  await wholePage(page);
  const far = await row(page, id).evaluate((r) => r.getBoundingClientRect().top + scrollY);
  // A window a fifth as tall as the list: what's in the page is three windows' worth, so the list reaches a window or more
  // past it wherever the page is, far enough for a row to come from outside and go back out.
  const listed = await page.locator(LIVE).evaluate((ul) => ul.getBoundingClientRect().height);
  await page.setViewportSize({ width: size.width, height: Math.min(size.height, Math.floor(listed / 5)) });
  // The page where the rows that need you end: where one that comes to need you lands (looked for again if the window, just sized, lets go of it meanwhile).
  await expect(async () => row(page, await firstNotWaiting(page)).scrollIntoViewIfNeeded()).toPass();
  await expect(row(page, id)).toHaveCount(0);
  // Left alone until the list follows its agents again (it holds still while it's in use: the page was just scrolled).
  await expect(page.locator("[data-held]")).toHaveCount(0);
  const trace = await watch(page, id);
  const over = async () => (await trace()).filter((t) => t.what === "over" || t.what === "cut").length;
  try {
    // It comes to need you: its row rises to the rows that do, into the window.
    await page.request.post("/api/mock/agent-patch", { data: { id, patch: { held: { why: "Look at the audit", since: Date.now() } } } });
    await expect(row(page, id)).toHaveAttribute("data-stand", "needs");
    const came = (await trace()).find((t) => t.what === "in");
    expect(came, "it was never put in the page").toBeTruthy();
    // From where it was, far under the window, to where it comes to rest: a glide, not a new row's fade.
    await expect.poll(over, { message: "its glide never ended" }).toBe(1);
    const now = await row(page, id).evaluate((r) => r.getBoundingClientRect().top + scrollY);
    expect(Math.abs(startsFrom(came) - (far - now)), "it didn't glide from where it was").toBeLessThan(1);
    expect(
      came?.frames.every((f) => f.opacity === undefined),
      "it faded in as a new row does",
    ).toBe(true);
    // The list follows its agents again; it needs you no more: back down the list it goes, out of the window, gliding out first.
    await expect(page.locator("[data-held]")).toHaveCount(0);
    const before = (await trace()).length;
    await page.request.post("/api/mock/agent-patch", { data: { id, patch: { held: null } } });
    await expect(row(page, id)).toHaveCount(0);
    const after = (await trace()).slice(before);
    // To where it was again, from where it stood among the rows that need you.
    expect(Math.abs(startsFrom(after.find((t) => t.what === "glide")) - (now - far)), "its glide doesn't start where it was").toBeLessThan(1);
    // Let go once its glide was over, not before, and not cut short (moved to its new place in the list first, it's put in there: no matter).
    expect(after.filter((t) => t.what !== "in").map((t) => t.what)).toEqual(["glide", "over", "out"]);
  } finally {
    await page.request.post("/api/mock/agent-patch", { data: { id, patch: { held: null } } });
  }
});

/** What the page shows at every frame from now until `stop()`: where the row `id` is in the viewport, whether its element is the one it was, and whether a gap shows. Read as each frame begins and again once it's drawn. */
type Shown = { top: number; same: boolean; gap: boolean };
async function everyFrame(page: Page, id: string) {
  await page.evaluate(
    ([list, id]) => {
      const ul = document.querySelector(list as string);
      const find = () => ul?.querySelector(`:scope > li.ms-row[data-agent="${id}"]`) ?? null;
      const was = find();
      const shown: Shown[] = [];
      const w = window as unknown as { rowShown: { shown: Shown[]; on: boolean } };
      w.rowShown = { shown, on: true };
      const look = () => {
        const r = find();
        const gap = [...(ul?.querySelectorAll(":scope > li[data-gap]") ?? [])].some((g) => {
          const b = g.getBoundingClientRect();
          return b.bottom > 0 && b.top < innerHeight;
        });
        shown.push({ top: r ? r.getBoundingClientRect().top : NaN, same: r === was, gap });
      };
      // After the frame's own work (what a resize or a scroll made of it), as it's drawn.
      const drawn = new MessageChannel();
      drawn.port1.onmessage = () => w.rowShown.on && look();
      const step = () => {
        if (!w.rowShown.on) return;
        look();
        drawn.port2.postMessage(0);
        requestAnimationFrame(step);
      };
      step();
    },
    [LIVE, id],
  );
  return () =>
    page.evaluate(
      () =>
        new Promise<Shown[]>((done) =>
          // The frame after the last change, too.
          requestAnimationFrame(() =>
            requestAnimationFrame(() => {
              const w = window as unknown as { rowShown: { shown: Shown[]; on: boolean } };
              w.rowShown.on = false;
              done(w.rowShown.shown);
            }),
          ),
        ),
    );
}

for (const anchored of [true, false]) {
  test(`W21-R02 what's in view holds still at every frame as a tall line above it is measured again, ${anchored ? "where the browser anchors the scroll" : "where it doesn't (as WebKit): the page is scrolled by hand"}`, async ({
    page,
  }) => {
    // A browser without scroll anchoring: it says it has none, and anchors nothing.
    if (!anchored)
      await page.addInitScript(() => {
        const supports = CSS.supports.bind(CSS) as (...args: string[]) => boolean;
        CSS.supports = ((...args: string[]) => (args[0] === "overflow-anchor" ? false : supports(...args))) as typeof CSS.supports;
        document.addEventListener("DOMContentLoaded", () => {
          const style = document.createElement("style");
          style.textContent = "* { overflow-anchor: none !important; }";
          document.head.append(style);
        });
      });
    // The tree's requests, so "Storage lead" needs you and its tree is a tall line.
    await page.request.post("/api/mock/tree-asks");
    try {
      await page.goto("/agents");
      await page.locator(`${LIVE} > li.ms-row`).first().waitFor();
      await settled(page);
      const full = page.viewportSize();
      if (!full) return;
      const lead = row(page, "tree-lead");
      const tree = page.locator(`${LIVE} > li.ms-tree`);
      /** Down the page, as someone looking for it would go, until Storage lead's row is in view. */
      const toLead = async () => {
        for (;;) {
          if ((await lead.count()) > 0) break;
          expect(await page.evaluate(() => (scrollBy(0, innerHeight / 2), scrollY + innerHeight < document.documentElement.scrollHeight)), "Storage lead isn't in the list").toBe(true);
        }
        await lead.scrollIntoViewIfNeeded();
      };
      await toLead();
      await lead.getByRole("button", { name: "Show Storage lead's subagents" }).click();
      await expect(tree).toHaveCount(1);
      const tall = await tree.evaluate((t) => t.getBoundingClientRect().height);
      const typical = middle((await boxes(page)).map((r) => r.height));
      expect(tall, "the tree is no taller than two rows").toBeGreaterThan(2 * typical);
      // A window half as tall as the tree: taken for a row, then measured as tall as it is, the tree moves what's under it by more than the window shows.
      const size = { width: full.width, height: Math.floor(tall / 2) };
      await page.setViewportSize(size);
      await toLead();
      await expect(tree).toHaveCount(1);
      // The focus out of its row: while it's there, the tree is where Tab goes next, and stays.
      await page.evaluate(() => (document.activeElement as HTMLElement | null)?.blur());
      // Past it by more than a viewport: the tree is out of the page (its row stays, its tree open).
      const past = await tree.evaluate((t) => t.getBoundingClientRect().bottom + scrollY + innerHeight + 1);
      await page.evaluate((y) => scrollTo(0, y), past);
      await expect(tree).toHaveCount(0);
      await expect(lead).toHaveCount(1);
      await expect.poll(() => gapShows(page)).toBe(false);
      // The first row wholly in the viewport: what's watched.
      const watched = await page.locator(`${LIVE} > li.ms-row`).evaluateAll((rows) => rows.find((r) => r.getBoundingClientRect().top >= 0)?.getAttribute("data-agent") ?? "");
      const at = () => row(page, watched).evaluate((r) => r.getBoundingClientRect().top);
      /** The room kept for the rows between Storage lead's row and those near the viewport: its tree's among them. */
      const roomAbove = () => page.locator(`${LIVE} > li.ms-row[data-agent="tree-lead"] + li[data-gap]`).evaluate((g) => g.getBoundingClientRect().height);
      // A pixel narrower: every height is measured again, and the tree, not in the page, is taken for a row until it is: the room over what's in view shrinks by the difference.
      const was = await at();
      const room = await roomAbove();
      let frames = await everyFrame(page, watched);
      await page.setViewportSize({ width: size.width - 1, height: size.height });
      // Drawn again once it's nearer what it comes to (less by the tree's height over a row's) than what it was.
      await expect.poll(roomAbove, { message: "the room above wasn't drawn again" }).toBeLessThan(room - (tall - typical) / 2);
      for (const f of await frames()) {
        expect(Math.abs(f.top - was), "what's in view moved as the room above it changed").toBeLessThan(1);
        expect([f.same, f.gap], "what's in view was taken out of the page as the room above it changed").toEqual([true, false]);
      }
      // Up to where the tree comes back into the window (not into view): measured as tall as it is, and what's in view goes only as far as it was scrolled.
      const by = Math.round(size.height / 2);
      const from = await at();
      frames = await everyFrame(page, watched);
      await page.evaluate((by) => scrollBy(0, -by), by);
      await expect(tree).toHaveCount(1);
      const shown = await frames();
      for (const f of shown) {
        expect(Math.min(Math.abs(f.top - from), Math.abs(f.top - (from + by))), "what's in view moved as the tree above it was measured").toBeLessThan(1);
        expect([f.same, f.gap], "what's in view was taken out of the page as the tree above it was measured").toEqual([true, false]);
      }
      expect(Math.abs((shown.at(-1)?.top ?? NaN) - (from + by))).toBeLessThan(1);
      expect(await tree.evaluate((t) => t.getBoundingClientRect().bottom < 0), "the tree is above the viewport").toBe(true);
    } finally {
      await page.request.post("/api/mock/asks");
    }
  });
}
