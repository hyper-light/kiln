/**
 * The Changes tab's Commits (commits.tsx; the Changes panel plan 01a194444b1d, step 4), on the
 * mock's fam-orch: its branch's commits since its trunk, what isn't committed yet on top, this
 * session's set apart and picked at first, the picked ones' files below. Picked as Mission control
 * picks agents: a tap shows one alone, Shift picks a run, ⌘/Ctrl adds or takes away one, the band's
 * ring picks all of the session's, and on a phone a press held and dragged picks or unpicks as the
 * first went, the list not scrolling under it. Where its machine can't list them, Commits is off
 * and says why.
 */
import { expect, test, type Page } from "@playwright/test";

/** Against a real kiln server (playwright.config.ts): these drive the mock's agents. */
const REAL = !!process.env.E2E_BASE_URL;

const SHA = {
  planner: "c4f1e2a",
  docs: "9b8e7d6",
  steps: "5d4c3b2",
  order: "1a2b3c4",
};

async function open(page: Page, agent = "fam-orch") {
  await page.goto(`/agents/${agent}?tab=changes`);
  const tab = page
    .getByRole("tab", { name: /^Changes/ })
    .filter({ visible: true })
    .first();
  await tab.click();
  await expect(page.locator(".cx-head")).toBeVisible();
}

async function commitsMode(page: Page) {
  const mode = page.getByRole("radio", { name: "Commits" });
  await expect(mode).toBeEnabled();
  await mode.click();
  await expect(page.locator(".cx-commits")).toBeVisible();
}

const row = (page: Page, sha: string) => page.locator(`.cx-commit[data-commit^="${sha}"]`);
/** The commits picked, by their short ids (`now` for what isn't committed yet), top to bottom. */
const picked = (page: Page) => page.locator(".cx-commit[aria-pressed=true] .cx-sha").allTextContents();
const files = (page: Page) => page.locator(".cx-file:visible .cx-path-name").allTextContents();

test("Commits lists its branch's commits, this session's picked at first, and what they change together", async ({ page }) => {
  test.skip(REAL, "the mock's agents");
  await open(page);
  await commitsMode(page);
  await expect(page.getByRole("button", { name: "On: plans/side-by-side" })).toBeVisible();
  // What isn't committed yet on top, then this session's, then what came before.
  await expect(page.locator(".cx-commit .cx-sha")).toHaveText(["now", SHA.planner, SHA.docs, SHA.steps, SHA.order]);
  await expect(page.locator(".cx-cband")).toHaveText(["This session", "Before this session"]);
  await expect.poll(() => picked(page)).toEqual([SHA.planner, SHA.docs]);
  await expect(page.getByRole("checkbox", { name: "Pick this session's commits" })).toHaveAttribute("aria-checked", "true");
  await expect.poll(() => files(page)).toEqual(["planner.rs", "plans.md"]);
  await expect(page.locator(".cx-total .hx-stat")).toHaveText("+6−1");
});

test("a tap shows one alone, Shift picks a run, ⌘/Ctrl adds or takes away one, the ring picks the session's", async ({ page }) => {
  test.skip(REAL, "the mock's agents");
  await open(page);
  await commitsMode(page);
  await row(page, SHA.steps).click();
  await expect.poll(() => picked(page)).toEqual([SHA.steps]);
  await expect.poll(() => files(page)).toEqual(["step.rs", "plans.md"]);
  await row(page, SHA.planner).click({ modifiers: ["Shift"] });
  await expect.poll(() => picked(page)).toEqual([SHA.planner, SHA.docs, SHA.steps]);
  await row(page, SHA.docs).click({ modifiers: ["ControlOrMeta"] });
  await expect.poll(() => picked(page)).toEqual([SHA.planner, SHA.steps]);
  // Some of the session's: the ring says so, and picks them all.
  const ring = page.getByRole("checkbox", { name: "Pick this session's commits" });
  await expect(ring).toHaveAttribute("aria-checked", "mixed");
  await ring.click();
  await expect.poll(() => picked(page)).toEqual([SHA.planner, SHA.docs, SHA.steps]);
  await ring.click();
  await expect.poll(() => picked(page)).toEqual([SHA.steps]);
  // With a gap the docs commit can't go without the one it needs, left out: it says which.
  await row(page, SHA.order).click();
  await row(page, SHA.docs).click({ modifiers: ["ControlOrMeta"] });
  await expect(page.locator(".cx-none")).toHaveText(`${SHA.docs} “Docs: steps side by side” needs ${SHA.steps} “Plans: a step says what it needs”, which isn't picked.`);
});

/** A press held on the row of `from`, dragged over to `to`'s, by touch: Chromium's own touches (CDP); WebKit's pointer events as a touch makes them. */
async function holdDrag(page: Page, from: string, to: string, browser: string) {
  const a = await row(page, from).boundingBox();
  const b = await row(page, to).boundingBox();
  if (!a || !b) throw new Error("rows not shown");
  const [x, y0, y1] = [a.x + a.width / 2, a.y + a.height / 2, b.y + b.height / 2];
  const steps = 6;
  if (browser === "chromium") {
    const cdp = await page.context().newCDPSession(page);
    const touch = (type: "touchStart" | "touchMove" | "touchEnd", y: number) => cdp.send("Input.dispatchTouchEvent", { type, touchPoints: type === "touchEnd" ? [] : [{ x, y }] });
    await touch("touchStart", y0);
    await page.waitForTimeout(650);
    for (let i = 1; i <= steps; i++) await touch("touchMove", y0 + ((y1 - y0) * i) / steps);
    await touch("touchEnd", y1);
    await cdp.detach();
    return;
  }
  await page.evaluate(
    async ({ sel, x, y0, y1, steps }) => {
      const el = document.querySelector(sel);
      if (!el) throw new Error(sel);
      const fire = (type: string, y: number) => {
        const target = type === "pointerdown" ? el : (document.elementFromPoint(x, y) ?? el);
        target.dispatchEvent(new PointerEvent(type, { bubbles: true, cancelable: true, pointerId: 7, pointerType: "touch", isPrimary: true, clientX: x, clientY: y }));
      };
      fire("pointerdown", y0);
      await new Promise((r) => setTimeout(r, 650));
      for (let i = 1; i <= steps; i++) fire("pointermove", y0 + ((y1 - y0) * i) / steps);
      fire("pointerup", y1);
    },
    { sel: `.cx-commit[data-commit^="${from}"]`, x, y0, y1, steps },
  );
}

test("by touch, a press held and dragged picks or unpicks as the first went, the list not scrolling", async ({ page, browserName, isMobile }) => {
  test.skip(REAL, "the mock's agents");
  test.skip(!isMobile, "a phone's touches");
  await open(page);
  // The mock's own notices sit over the foot of a phone's screen: out of the finger's way.
  await page.addStyleTag({ content: ".toast-viewport,.rq-viewport{display:none!important}" });
  await commitsMode(page);
  await expect.poll(() => picked(page)).toEqual([SHA.planner, SHA.docs]);
  // The list at the top of the screen, its last row in reach (a phone's screen is short).
  await page.locator(".cx-commits").evaluate((e) => e.scrollIntoView({ block: "start" }));
  const top = async () => (await row(page, "now").boundingBox())?.y ?? NaN;
  // Held on a picked one: it and the rest passed are unpicked.
  await holdDrag(page, SHA.planner, SHA.steps, browserName);
  await expect.poll(() => picked(page)).toEqual([]);
  await expect(page.locator(".cx-none")).toHaveText("Pick a commit to see what it changed.");
  // Held on one that isn't: it and the rest passed are picked, the page not moving under the finger
  // (measured as what's under the list only grows: what shrinks below can pull a page scrolled down).
  const before = await top();
  await holdDrag(page, SHA.docs, SHA.order, browserName);
  await expect.poll(() => picked(page)).toEqual([SHA.docs, SHA.steps, SHA.order]);
  expect(await top(), "the list didn't scroll under the finger").toBe(before);
});

test("where its machine can't list its commits, Commits is off and says why", async ({ page }) => {
  test.skip(REAL, "the mock's agents");
  await open(page, "fam-tests");
  const mode = page.getByRole("radio", { name: "Commits" });
  await expect(mode).toBeDisabled();
  await expect(mode).toHaveAttribute("title", /isn't in a git repository/);
});
