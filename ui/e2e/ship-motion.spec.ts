/**
 * Ship without motion and without colour (Agent pace and checks plan
 * 01a175b7f2837279be2a229ee94ac1f0, step 3; WCAG 2.2.2 and 1.4.1, the plan's findings f4 and f3),
 * against the mock. With the device asking for reduced motion, nothing on Ship moves while commits
 * arrive, a row opens and Diff brings its Files into view: no animation or transition runs, the
 * stands' rule holds still, the list doesn't glide and the page jumps rather than scrolls. In
 * grayscale (Chromium's achromatopsia, a grayscale filter in WebKit), every state Ship draws, on its
 * list, in an opened row and on a commit's page, is told from every other by its glyph's shape, and
 * names itself in words.
 */
import { expect, test, type Page } from "@playwright/test";
import { STATES } from "../src/components/ui/status-glyph-spec";

const REAL = !!process.env.E2E_BASE_URL;
test.skip(REAL, "drives the mock's commits");
test.use({ reducedMotion: "reduce" });

/** Each animation that ran (CSS animations and transitions, the Web Animations API) with time to take, by frame. */
const watchMotion = (page: Page) =>
  page.addInitScript(() => {
    const w = window as unknown as { ran: Set<string> };
    w.ran = new Set();
    const name = (t: Element | null) => (t ? `${t.tagName.toLowerCase()}${typeof t.className === "string" && t.className.trim() ? `.${t.className.trim().split(/\s+/).join(".")}` : ""}` : "?");
    const tick = () => {
      for (const a of document.getAnimations()) {
        if (a.playState !== "running") continue;
        const ms = Number(a.effect?.getComputedTiming().activeDuration ?? 0);
        // Reduced motion's 0.01 ms (index.css): over before the frame is drawn.
        if (!(ms > 1)) continue;
        const what = a instanceof CSSAnimation ? a.animationName : a instanceof CSSTransition ? a.transitionProperty : "animate()";
        w.ran.add(`${what} on ${name((a.effect as KeyframeEffect | null)?.target ?? null)} (${Math.round(ms)} ms)`);
      }
      requestAnimationFrame(tick);
    };
    requestAnimationFrame(tick);
  });

test("with reduced motion, nothing on Ship moves as commits arrive, a row opens and Diff brings its Files into view", async ({ page }) => {
  test.setTimeout(90_000);
  await page.request.post("/api/mock/ship/arrive?clear=1");
  await watchMotion(page);
  try {
    await page.goto("/ship");
    const list = page.getByRole("list", { name: "Commits", exact: true });
    await expect(list.locator(":scope > li").first()).toBeVisible({ timeout: 15_000 });
    // The device's setting is the page's one switch: every animation looks at it.
    await expect(page.locator("html")).toHaveAttribute("data-motion", "paused");
    // The stands' rule, a canvas: drawn at rest, and not again while its counts hold.
    const rule = page.locator("canvas.sd-rule").first();
    await expect(rule).toBeVisible();
    const drawn = () => rule.evaluate((c) => (c as HTMLCanvasElement).toDataURL());
    const still = await drawn();
    await page.waitForTimeout(2_500);
    expect(await drawn(), "the rule held still").toBe(still);
    // Commits arrive and the list takes a new order, the pointer off it: rows take their places at once.
    await page.request.post("/api/mock/ship/agents?n=6&over=3");
    await expect(list.locator(":scope > li").filter({ hasText: "release: production waits on you" })).toBeVisible({ timeout: 20_000 });
    await expect(list.locator(":scope > li").filter({ hasText: "agent: asks about flake 4" })).toBeVisible({ timeout: 20_000 });
    // A row opens by its Diff (Review) button and its Files come into view: by one jump, not a scroll.
    await page.evaluate(() => {
      const w = window as unknown as { ys: number[] };
      w.ys = [];
      const f = () => {
        w.ys.push(Math.round(window.scrollY));
        if (w.ys.length < 240) requestAnimationFrame(f);
      };
      requestAnimationFrame(f);
    });
    const row = list.locator(":scope > li").filter({ hasText: "D41" }).first();
    await row.getByRole("button", { name: "Review", exact: true }).click();
    await expect(row.locator('section[aria-label="Files"]')).toBeFocused({ timeout: 15_000 });
    await page.waitForTimeout(1_500);
    const ys = await page.evaluate(() => (window as unknown as { ys: number[] }).ys);
    const moves = ys.filter((y, i) => i > 0 && y !== ys[i - 1]).length;
    expect(moves, `the page's scroll, frame by frame: ${[...new Set(ys)].join(", ")}`).toBeLessThanOrEqual(2);
    // And through it all, nothing animated.
    await page.waitForTimeout(1_000);
    expect(await page.evaluate(() => [...(window as unknown as { ran: Set<string> }).ran]), "what moved").toEqual([]);
  } finally {
    await page.request.post("/api/mock/ship/arrive?clear=1");
  }
});

/** One glyph of each state on the page: the state's word and the glyph as drawn. */
const glyphsOn = (page: Page) =>
  page.locator("svg.sg[data-state]").evaluateAll((els) => els.map((el) => ({ state: el.getAttribute("data-state") ?? "", word: el.getAttribute("aria-label") ?? "", html: el.outerHTML })));

test("in grayscale, every state Ship draws is told from every other by its glyph's shape, and in words", async ({ page, browserName, isMobile }) => {
  test.skip(isMobile, "glyphs draw the same at every width: the desktop projects check them");
  test.setTimeout(150_000);
  // Every state Ship draws: its list (each row's journey, and the mock's commits whose journeys run
  // through every state a glyph has), opened rows (Lanes, the job cards), and commits' pages (Build,
  // Replant, Environments).
  await page.request.post("/api/mock/ship/arrive?clear=1");
  await page.request.post("/api/mock/ship/glyphs");
  const found = new Map<string, { word: string; html: string }>();
  const take = async () => {
    for (const g of await glyphsOn(page)) if (g.state && !found.has(g.state)) found.set(g.state, g);
  };
  await page.goto("/ship");
  const list = page.getByRole("list", { name: "Commits", exact: true });
  await expect(list.locator(":scope > li").first()).toBeVisible({ timeout: 15_000 });
  await take();
  for (const name of ["D41", "sync: a move carries its homes", "ci: a landed change"]) {
    const row = list.locator(":scope > li").filter({ hasText: name }).first();
    const go = row.locator(".ship-go");
    if ((await go.getAttribute("aria-expanded")) !== "true") await go.click();
    await expect(row.locator('section[aria-label="Lanes"] .ship-lanesbox')).toBeVisible({ timeout: 15_000 });
    await take();
    await go.click();
  }
  const commits = ((await (await page.request.get("/api/ship/commits")).json()) as { commits: { repo: string; sha: string }[] }).commits;
  for (const c of commits) {
    await page.goto(`/ship/${c.repo}/${c.sha}`);
    await expect(page.getByRole("region", { name: "Commit" })).toBeVisible({ timeout: 15_000 });
    await expect(page.getByRole("region", { name: "Stages" })).toBeVisible({ timeout: 15_000 });
    await take();
  }
  await page.request.post("/api/mock/ship/arrive?clear=1");
  expect([...found.keys()].sort(), "every state a glyph has, drawn on Ship").toEqual([...STATES].sort());
  // Each in words, its own.
  const words = [...found.values()].map((g) => g.word);
  expect(
    words.filter((w) => !w.trim()),
    "a glyph with no word",
  ).toEqual([]);
  expect(new Set(words).size, words.join(", ")).toBe(words.length);
  // In grayscale, each state's glyph alone on one ground, four times its size, drawn as Ship draws it.
  await page.goto("/ship");
  await expect(list.locator(":scope > li").first()).toBeVisible({ timeout: 15_000 });
  if (browserName === "chromium") await (await page.context().newCDPSession(page)).send("Emulation.setEmulatedVisionDeficiency", { type: "achromatopsia" });
  else await page.addStyleTag({ content: "html { filter: grayscale(1) }" });
  const states = [...found.keys()];
  // A control: the first state's glyph twice. The measure must call those two alike.
  const tiles = [...states, "control"];
  await page.evaluate(
    (tiles) => {
      const lab = document.createElement("div");
      lab.id = "gray-lab";
      lab.style.cssText = "position:fixed;left:0;top:0;z-index:2147483647;display:flex;flex-wrap:wrap;gap:4px;padding:4px;background:#0b0c0e;width:100vw";
      for (const [state, html] of tiles) {
        const tile = document.createElement("div");
        tile.dataset.tile = state;
        tile.style.cssText = "width:60px;height:60px;display:grid;place-items:center;background:#0b0c0e";
        tile.innerHTML = html;
        const svg = tile.querySelector("svg");
        svg?.setAttribute("width", "56");
        svg?.setAttribute("height", "56");
        lab.append(tile);
      }
      document.body.append(lab);
    },
    tiles.map((s) => [s, found.get(s === "control" ? (states[0] ?? "") : s)?.html ?? ""]),
  );
  // Each tile's luminance, read back from its grayscale screenshot.
  const shots: string[] = [];
  for (const s of tiles) shots.push((await page.locator(`#gray-lab [data-tile="${s}"]`).screenshot()).toString("base64"));
  const differ = await page.evaluate(async (pngs) => {
    const lum = async (b64: string) => {
      const img = new Image();
      img.src = `data:image/png;base64,${b64}`;
      await img.decode();
      const c = document.createElement("canvas");
      [c.width, c.height] = [img.width, img.height];
      const x = c.getContext("2d");
      if (!x) return [];
      x.drawImage(img, 0, 0);
      const d = x.getImageData(0, 0, img.width, img.height).data;
      const out: number[] = [];
      for (let i = 0; i < d.length; i += 4) out.push(0.299 * (d[i] ?? 0) + 0.587 * (d[i + 1] ?? 0) + 0.114 * (d[i + 2] ?? 0));
      return out;
    };
    const ls = await Promise.all(pngs.map(lum));
    // For each pair, how many pixels differ by more than a tenth of the range.
    return ls.map((a) => ls.map((b) => a.reduce((n, v, i) => n + (Math.abs(v - (b[i] ?? 0)) > 25 ? 1 : 0), 0)));
  }, shots);
  expect(differ[0]?.[states.length] ?? Number.NaN, `${states[0]} beside itself`).toBeLessThan(30);
  const alike: string[] = [];
  let closest = { n: Number.POSITIVE_INFINITY, pair: "" };
  for (let i = 0; i < states.length; i++)
    for (let j = i + 1; j < states.length; j++) {
      const n = differ[i]?.[j] ?? 0;
      if (n < closest.n) closest = { n, pair: `${states[i]} and ${states[j]}` };
      if (n < 30) alike.push(`${states[i]} and ${states[j]} (${n} pixels apart)`);
    }
  const said = `${states.length} states (${states.join(" ")}); the closest, ${closest.pair}, ${closest.n} pixels apart of ${60 * 60}`;
  test.info().annotations.push({ type: "grayscale", description: said });
  console.log(`grayscale: ${said}`);
  expect(alike, "states that look alike in grayscale").toEqual([]);
});
