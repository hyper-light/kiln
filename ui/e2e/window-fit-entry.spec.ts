/** W21-R13: inspect entry before its first frame and the real new-agent view-transition callback.
 * These geometry reads are correctness probes, not performance measurements or pixel comparisons.
 * Browser viewport emulation does not prove native keyboard/zoom behavior on a physical phone.
 */
import { expect, test, type Page } from "@playwright/test";

interface Geometry {
  at: number;
  route: string;
  expected: number;
  fitted: boolean;
  rootHeight: string;
  short: boolean;
  viewportWidth: number;
  pageWidth: number;
  surface: "agent" | "mission" | "other";
  loading: boolean;
  mainClass: string | null;
  main: { top: number; bottom: number; width: number; height: number } | null;
}
interface NamedPart {
  name: string;
  owner: "mission" | "agent";
  x: number;
  y: number;
  width: number;
  height: number;
}
interface Snapshot {
  geometry: Geometry;
  opening: boolean;
  run: NamedPart[];
  composer: NamedPart[];
}
interface Transition {
  before: Snapshot;
  afterUpdate?: Snapshot;
  ready?: Snapshot;
  finished?: Snapshot;
  readyError?: string;
  updateError?: string;
  finishedError?: string;
}
interface EntryProbe {
  beforeAgent: Geometry | null;
  firstMutation: Geometry | null;
  firstFrame: Geometry | null;
  transitions: Transition[];
  supported: boolean;
  animations: { name: string; duration: string; ended?: boolean }[];
}

async function observeEntry(page: Page) {
  await page.emulateMedia({ reducedMotion: "no-preference" });
  await page.addInitScript(() => {
    const probe: EntryProbe = { beforeAgent: null, firstMutation: null, firstFrame: null, transitions: [], supported: false, animations: [] };
    Object.assign(window, { __windowEntry: probe });
    const geometry = (): Geometry => {
      const root = document.documentElement;
      const view = window.visualViewport;
      const main = document.getElementById("main");
      const agent = main?.querySelector(".hx-agent");
      const box = main?.getBoundingClientRect();
      return {
        at: performance.now(),
        route: location.pathname,
        expected: Math.round(view ? view.height * view.scale : innerHeight),
        fitted: root.classList.contains("fills-window"),
        rootHeight: root.style.getPropertyValue("--window-h"),
        short: root.dataset.window === "short",
        viewportWidth: innerWidth,
        pageWidth: root.scrollWidth,
        surface: agent ? "agent" : main?.querySelector(".mc") ? "mission" : "other",
        loading: agent?.getAttribute("aria-busy") === "true",
        mainClass: main?.className ?? null,
        main: box ? { top: box.top, bottom: box.bottom, width: box.width, height: box.height } : null,
      };
    };
    new MutationObserver(() => {
      if (probe.firstMutation || !(location.pathname === "/agents/demo-claude" || /^\/agents\/[0-9a-f]{32}$/.test(location.pathname))) return;
      if (!document.querySelector("#main .hx-agent")) {
        // History and the shell can name the destination while Suspense retains Mission control.
        // Keep that evidence, but start the agent-entry check at its actual DOM, including its loader.
        if (!probe.beforeAgent && document.querySelector("main.relative.flex")) probe.beforeAgent = geometry();
        return;
      }
      // Mutation observers run after the mounting commit and before rendering the next frame.
      probe.firstMutation = geometry();
      requestAnimationFrame(() => {
        probe.firstFrame = geometry();
      });
    }).observe(document, { subtree: true, childList: true, attributes: true });
    document.addEventListener("animationstart", (event) => {
      if (event.animationName === "ag-ready" && (event.target as Element)?.classList.contains("hx-agent"))
        probe.animations.push({ name: event.animationName, duration: getComputedStyle(event.target as Element).animationDuration });
    });
    document.addEventListener("animationend", (event) => {
      if (event.animationName === "ag-ready") {
        const animation = probe.animations.at(-1);
        if (animation) animation.ended = true;
      }
    });
    const named = (selector: string): NamedPart[] =>
      [...document.querySelectorAll<HTMLElement>(selector)].flatMap((el) => {
        const rect = el.getBoundingClientRect();
        if (!rect.width || !rect.height) return [];
        return [{ name: getComputedStyle(el).viewTransitionName, owner: el.closest(".hx-agent") ? "agent" : "mission", x: rect.x, y: rect.y, width: rect.width, height: rect.height }];
      });
    const snapshot = (): Snapshot => ({
      geometry: geometry(),
      opening: document.documentElement.hasAttribute("data-opening"),
      run: named(".mc .mc-how, .hx-agent .ag-run"),
      composer: named(".mc .hx-compose-box, .hx-agent .hx-dock .hx-compose-box"),
    });
    type Update = (() => unknown) | { update?: () => unknown; types?: string[] };
    type NativeTransition = { ready: Promise<void>; updateCallbackDone: Promise<void>; finished: Promise<void> };
    const doc = document as unknown as { startViewTransition?: (update: Update) => NativeTransition };
    if (!doc.startViewTransition) return;
    probe.supported = true;
    const native = doc.startViewTransition.bind(document);
    Object.defineProperty(document, "startViewTransition", {
      configurable: true,
      value: (input: Update) => {
        const record: Transition = { before: snapshot() };
        probe.transitions.push(record);
        const update = typeof input === "function" ? input : input.update;
        const wrapped = async () => {
          await update?.();
          // This is still inside the API's update callback, before its new snapshot is taken.
          record.afterUpdate = snapshot();
        };
        const transition = native(typeof input === "function" ? wrapped : { ...input, update: wrapped });
        void transition.updateCallbackDone.catch((error: unknown) => {
          record.updateError = String(error);
        });
        void transition.ready.then(
          () => {
            record.ready = snapshot();
            const capture = (window as unknown as { __entryTransitionScreenshot?: (meta: unknown) => Promise<void> }).__entryTransitionScreenshot;
            void capture?.({ at: performance.now(), phase: "ready", route: location.pathname }).catch(() => {});
          },
          (error: unknown) => {
            record.readyError = String(error);
          },
        );
        void transition.finished.then(
          () => {
            record.finished = snapshot();
          },
          (error: unknown) => {
            record.finishedError = String(error);
          },
        );
        return transition;
      },
    });
  });
}

const readProbe = (page: Page) => page.evaluate(() => (window as unknown as { __windowEntry: EntryProbe }).__windowEntry);

function fitted(value: Geometry | null | undefined) {
  expect(value, "the entry phase was observed").toBeTruthy();
  expect(value!.surface, "the phase contains the destination agent, including its loading frame").toBe("agent");
  expect(value!.fitted).toBe(true);
  expect(value!.rootHeight).toBe(`${value!.expected}px`);
  expect(value!.short).toBe(value!.expected < 560);
  expect(value!.pageWidth).toBeLessThanOrEqual(value!.viewportWidth);
  expect(value!.main?.height).toBeGreaterThan(0);
  expect(value!.main!.bottom).toBeLessThanOrEqual(value!.expected + 2);
}

test.beforeEach(() => {
  test.skip(!!process.env.E2E_BASE_URL, "mock-only agent entry and creation");
});

test.afterEach(async ({ page }, info) => {
  // Always retain the pre-frame and native-snapshot records, including when an assertion fails.
  try {
    await info.attach("window-entry-probe", { body: JSON.stringify(await readProbe(page), null, 2), contentType: "application/json" });
    if (info.status !== info.expectedStatus) await info.attach("window-entry-failure", { body: await page.screenshot({ animations: "allow", timeout: 10_000 }), contentType: "image/png" });
  } catch (error) {
    await info.attach("window-entry-diagnostics-unavailable", { body: String(error), contentType: "text/plain" });
  }
});

test("W21-R13 the mounting commit and first animation frame already have the correct agent viewport", async ({ page }, info) => {
  await observeEntry(page);
  await page.goto("/agents/demo-claude", { waitUntil: "domcontentloaded" });
  await expect(page.getByRole("textbox", { name: "Message to the agent", exact: true })).toBeEnabled({ timeout: 30_000 });
  await expect.poll(async () => (await readProbe(page)).firstFrame !== null, { timeout: 30_000 }).toBe(true);
  const probe = await readProbe(page);
  fitted(probe.firstMutation);
  fitted(probe.firstFrame);
  await expect.poll(async () => (await readProbe(page)).animations.some((a) => a.name === "ag-ready" && a.duration === "0.18s")).toBe(true);
  await expect(page.locator("html")).toHaveAttribute("data-motion", "playing");
  await info.attach("first-agent-frame", { body: JSON.stringify(await readProbe(page), null, 2), contentType: "application/json" });
  await info.attach("agent-entry", { body: await page.screenshot({ animations: "allow" }), contentType: "image/png" });
});

test("W21-R13 creating from Mission control gives the real view transition fitted destination snapshots", async ({ page }, info) => {
  let capture: Promise<void> | null = null;
  await page.exposeBinding("__entryTransitionScreenshot", (_source, metadata: unknown) => {
    capture ??= (async () => {
      await info.attach("transition-ready-request", { body: JSON.stringify(metadata), contentType: "application/json" });
      // Requested at ready; browser/driver scheduling can finish the 220ms transition before the
      // screenshot completes. The snapshots/geometry assertions do not depend on this capture.
      try {
        await info.attach("new-agent-transition-ready-attempt", { body: await page.screenshot({ animations: "allow", timeout: 10_000 }), contentType: "image/png" });
      } catch (error) {
        await info.attach("transition-screenshot-unavailable", { body: String(error), contentType: "text/plain" });
      }
    })();
    return capture;
  });
  await observeEntry(page);
  await page.goto("/agents");
  const composer = page.getByRole("region", { name: "New agent", exact: true });
  const field = composer.getByRole("textbox", { name: "Its first message", exact: true });
  await field.fill("Check viewport fitting while this mock agent opens.");
  const start = composer.getByRole("button", { name: /^Start:/ });
  await expect(start).toBeEnabled();
  await expect(page.locator("html")).toHaveAttribute("data-motion", "playing");
  const response = page.waitForResponse((r) => new URL(r.url()).pathname === "/api/harness/sessions" && r.request().method() === "POST");
  await start.click();
  const created = await response;
  expect(created.ok()).toBe(true);
  expect(created.request().postDataJSON()).toMatchObject({ count: 1, spec: { prompt: "Check viewport fitting while this mock agent opens." } });
  // W21-R15: the current mock, like the server, starts under the id the page made.
  const made = (await created.json())[0].id as string;
  expect(made).toMatch(/^[0-9a-f]{32}$/);
  expect(created.request().postDataJSON().ids).toEqual([made]);
  await expect(page).toHaveURL(new RegExp(`/agents/${made}$`));
  await expect(page.getByRole("textbox", { name: "Message to the agent", exact: true })).toBeEnabled({ timeout: 30_000 });
  await expect.poll(async () => (await readProbe(page)).firstFrame !== null, { timeout: 30_000 }).toBe(true);
  let probe = await readProbe(page);
  fitted(probe.firstMutation);
  fitted(probe.firstFrame);
  if (probe.supported) {
    await expect.poll(async () => (await readProbe(page)).transitions[0]?.finished !== undefined).toBe(true);
    probe = await readProbe(page);
    expect(probe.transitions).toHaveLength(1);
    const transition = probe.transitions[0]!;
    expect(transition.updateError).toBeUndefined();
    expect(transition.readyError, "native snapshot collection accepted both named parts").toBeUndefined();
    expect(transition.finishedError).toBeUndefined();
    // The current router publishes its destination before replacing Mission control. The
    // c576e0957a38 baseline also fits the shell then; inspect the old surface and named parts,
    // rather than treating that global flag as proof that the agent has mounted.
    expect(transition.before.geometry.surface).toBe("mission");
    expect(transition.before.geometry.pageWidth).toBeLessThanOrEqual(transition.before.geometry.viewportWidth);
    expect(transition.before.geometry.main?.height).toBeGreaterThan(0);
    for (const [snapshot, owner] of [
      [transition.before, "mission"],
      [transition.afterUpdate, "agent"],
      [transition.ready, "agent"],
    ] as const) {
      expect(snapshot?.opening).toBe(true);
      expect(snapshot!.run).toHaveLength(1);
      expect(snapshot!.run[0]!.name).toBe("kiln-run");
      expect(snapshot!.run[0]!.owner).toBe(owner);
      expect(snapshot!.composer).toHaveLength(1);
      expect(snapshot!.composer[0]!.name).toBe("kiln-composer");
      expect(snapshot!.composer[0]!.owner).toBe(owner);
      expect(snapshot!.composer[0]!.width).toBeGreaterThan(0);
      expect(snapshot!.composer[0]!.height).toBeGreaterThan(0);
    }
    fitted(transition.afterUpdate?.geometry);
    fitted(transition.ready?.geometry);
    fitted(transition.finished?.geometry);
    if (capture) await capture;
  } else info.annotations.push({ type: "capability", description: "This browser has no native View Transition API; fitted entry was checked through the navigation fallback." });
  await expect.poll(async () => (await readProbe(page)).animations.some((a) => a.name === "ag-ready" && a.duration === "0.18s")).toBe(true);
  await info.attach("new-agent-snapshot-phases", { body: JSON.stringify(await readProbe(page), null, 2), contentType: "application/json" });
  await info.attach("new-agent-settled", { body: await page.screenshot({ animations: "allow" }), contentType: "image/png" });
});
