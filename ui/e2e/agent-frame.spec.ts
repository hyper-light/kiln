/** W21-R02/W21-R13: direct session navigation retains geometry, never a previous controller.
 * Browser history.pushState is the real subscribed TanStack history API, not a test-only router.
 * These are identity, first-frame and native-animation checks; no timing benchmark is implied.
 */
import { gunzipSync } from "node:zlib";
import { expect, test, type Page } from "@playwright/test";

const A = "tree-lead";
const B = "demo-claude";
const NAMES: Record<string, string> = { [A]: "Frame Alpha", [B]: "Frame Beta" };
const DRAFTS = { [A]: "Alpha's retained draft.", [B]: "Beta's independent draft." };
interface Wire {
  t?: string;
  id?: string;
  info?: Record<string, unknown>;
  meta?: { features?: string[]; [key: string]: unknown };
  state?: Record<string, unknown>;
  events?: unknown[];
  c?: { type?: string; text?: string; count?: number; reply_to?: string; attachments?: { name: string; file_id?: string }[] };
}
interface Entry {
  session: string | undefined;
  loading: boolean;
  header: boolean;
  draft: string | null;
  disabled: boolean;
  fitted: boolean;
  bottom: number;
  height: number;
  viewport: number;
  pageWidth: number;
  viewportWidth: number;
  grid: boolean;
  sideWidth: number;
  tabs: number;
}
interface SocketProbe {
  session: string;
  connectedAt: number;
  received: number;
  sent: number;
  pending: number;
  types: string[];
  complete: boolean;
  closedAt?: number;
  errors: string[];
}
const socketDiagnostics = new WeakMap<Page, SocketProbe[]>();

interface Probe {
  entries: Entry[];
  animations: { type: string; id: string | undefined; name: string; elapsed: number; duration: string }[];
  observed: number[];
}

async function setup(page: Page, hold = new Set<string>()) {
  const commands: { session: string; frame: Wire }[] = [];
  const release = new Map<string, () => void>();
  const captured = new Set<string>();
  const sockets: SocketProbe[] = [];
  socketDiagnostics.set(page, sockets);
  page.on("websocket", (socket) => {
    const session = new URL(socket.url()).pathname.split("/").at(-2);
    const diagnostic = () => sockets.findLast((entry) => entry.session === session);
    socket.on("socketerror", (error) => diagnostic()?.errors.push(error));
    socket.on("close", () => {
      const entry = diagnostic();
      if (entry) entry.closedAt = Date.now();
    });
  });
  await page.emulateMedia({ reducedMotion: "no-preference" });
  await page.addInitScript((drafts) => {
    sessionStorage.setItem("kiln.agents.drafts", JSON.stringify({ state: { drafts }, version: 0 }));
    localStorage.setItem("kiln.agents.inspector.collapsed", "false");
    const probe: Probe = { entries: [], animations: [], observed: [] };
    Object.assign(window, { __agentFrameProbe: probe });
    let sequence = 0;
    const Original = window.ResizeObserver;
    window.ResizeObserver = class extends Original {
      readonly frameObserverId: number;
      constructor(callback: ResizeObserverCallback) {
        super(callback);
        this.frameObserverId = ++sequence;
      }
      observe(target: Element, options?: ResizeObserverOptions) {
        if (target.matches(".hx-desktop")) probe.observed.push(this.frameObserverId);
        return super.observe(target, options);
      }
    };
    const seen = new Set<string>();
    new MutationObserver(() => {
      const root = document.querySelector<HTMLElement>("[data-agent-frame]");
      const id = root?.dataset.agentSession;
      if (!root || !id || seen.has(id)) return;
      seen.add(id);
      requestAnimationFrame(() => {
        const input = root.querySelector<HTMLTextAreaElement>('textarea[aria-label="Message to the agent"]');
        const box = root.getBoundingClientRect();
        const side = root.querySelector<HTMLElement>(".hx-desktop-tray");
        const sideBox = side?.getBoundingClientRect();
        probe.entries.push({
          session: root.dataset.agentSession,
          loading: root.getAttribute("aria-busy") === "true",
          header: !!root.querySelector(".ag-head .ag-title"),
          draft: input?.value ?? null,
          disabled: !!input?.disabled,
          fitted: document.documentElement.classList.contains("fills-window"),
          bottom: box.bottom,
          height: box.height,
          viewport: innerHeight,
          pageWidth: document.documentElement.scrollWidth,
          viewportWidth: innerWidth,
          grid: !!root.querySelector(".hx-desktop"),
          sideWidth: sideBox?.width ?? 0,
          tabs: root.querySelectorAll(".hx-agent-phone .pt button").length,
        });
      });
    }).observe(document, { childList: true, subtree: true, attributes: true });
    for (const type of ["animationstart", "animationend"])
      document.addEventListener(type, (event) => {
        const e = event as AnimationEvent;
        const root = e.target as HTMLElement;
        if (e.animationName !== "ag-ready" || !root.matches("[data-agent-frame]")) return;
        probe.animations.push({ type, id: root.dataset.agentSession, name: e.animationName, elapsed: e.elapsedTime, duration: getComputedStyle(root).animationDuration });
      });
  }, DRAFTS);
  await page.routeWebSocket(
    (url) => [A, B].some((id) => url.pathname === `/api/harness/sessions/${id}/ws`),
    (ws) => {
      const id = new URL(ws.url()).pathname.split("/").at(-2)!;
      const server = ws.connectToServer();
      const diagnostic: SocketProbe = { session: id, connectedAt: Date.now(), received: 0, sent: 0, pending: 0, types: [], complete: false, errors: [] };
      sockets.push(diagnostic);
      const pending: string[] = [];
      // Hold the whole snapshot before "open": the existing five-second incomplete-snapshot
      // safety cap must not turn a paced test runner's setup delay into an unintended release.
      let held = hold.has(id);
      let complete = false;
      release.set(id, () => {
        held = false;
        hold.delete(id);
        for (const line of pending.splice(0)) {
          ws.send(line);
          diagnostic.sent += 1;
        }
        diagnostic.pending = 0;
      });
      server.onMessage((message) => {
        if (complete) return;
        diagnostic.received += 1;
        let text: string;
        try {
          text = typeof message === "string" ? message : gunzipSync(message).toString();
        } catch (error) {
          diagnostic.errors.push(String(error));
          throw error;
        }
        for (const line of text.split("\n").filter(Boolean)) {
          const frame = JSON.parse(line) as Wire;
          diagnostic.types.push(frame.t ?? "unknown");
          if (frame.info) frame.info = { ...frame.info, name: NAMES[id], status: "idle" };
          if (frame.meta) frame.meta = { ...frame.meta, features: [...new Set([...(frame.meta.features ?? []), "attachments"])] };
          if (frame.state) frame.state = { ...frame.state, name: NAMES[id], status: "idle", tasks: [], queue: [], approvals: [], turn_started: null };
          if (frame.t === "events")
            frame.events = Array.from({ length: 40 }, (_, i) => [
              { id: `${id}-prompt-${i}`, seq: i * 2 + 1, at: 1_790_000_000_000 + i * 1000, kind: "user", text: `${NAMES[id]} request ${i}.` },
              { id: `${id}-answer-${i}`, seq: i * 2 + 2, at: 1_790_000_000_500 + i * 1000, kind: "assistant", text: `${NAMES[id]} answer ${i}. This conversation belongs only to ${id}.` },
            ]).flat();
          if (held) pending.push(JSON.stringify(frame));
          else {
            ws.send(JSON.stringify(frame));
            diagnostic.sent += 1;
          }
          diagnostic.pending = pending.length;
          if (frame.t === "synced") {
            captured.add(id);
            complete = true;
            diagnostic.complete = true;
          }
        }
      });
      ws.onMessage((message) => {
        const frame = JSON.parse(String(message)) as Wire;
        if (frame.t === "cmd" && ["send", "max_subagents"].includes(frame.c?.type ?? "")) {
          commands.push({ session: id, frame });
          ws.send(JSON.stringify({ t: "ack", id: frame.id, outcome: "taken" }));
        } else server.send(message);
      });
    },
  );
  return { commands, captured, release: (id: string) => release.get(id)?.() };
}

async function navigate(page: Page, id: string) {
  await page.evaluate((id) => {
    const previous = history.state ?? {};
    history.pushState({ ...previous, __TSR_index: (Number(previous.__TSR_index) || 0) + 1, __TSR_key: crypto.randomUUID() }, "", `/agents/${id}`);
  }, id);
}
const field = (page: Page) => page.getByRole("textbox", { name: "Message to the agent", exact: true });
async function ready(page: Page, id: string) {
  await expect(page.locator(".ag-title")).toHaveText(NAMES[id], { timeout: 30_000 });
  await expect(field(page)).toBeEnabled({ timeout: 30_000 });
  await expect(page.locator(`.hx-outcome[data-ev="${id}-answer-39"]`)).toContainText(`${NAMES[id]} answer 39.`);
}
const probe = (page: Page) => page.evaluate(() => (window as unknown as { __agentFrameProbe: Probe }).__agentFrameProbe);
async function animationEnd(page: Page, id: string, count: number) {
  await expect.poll(async () => (await probe(page)).animations.filter((event) => event.type === "animationend" && event.id === id).length, { timeout: 30_000 }).toBe(count);
}
async function settings(page: Page, id: string) {
  await page.getByRole("button", { name: `More for ${NAMES[id]}`, exact: true }).click();
  await page.getByRole("menuitem", { name: "Settings…", exact: true }).click();
  await expect(page.getByRole("dialog", { name: "Agent settings", exact: true })).toBeVisible();
}

test.beforeEach(() => test.skip(!!process.env.E2E_BASE_URL, "controlled mock-only snapshots and native animation checks"));
test.afterEach(async ({ page }, info) => {
  await info.attach("agent-frame-sockets", { body: JSON.stringify(socketDiagnostics.get(page) ?? [], null, 2), contentType: "application/json" });
  try {
    await info.attach("agent-frame-probe", { body: JSON.stringify(await probe(page), null, 2), contentType: "application/json" });
  } catch {
    // Preserve the original failure when setup did not reach the document.
  }
});

test("W21-R02 a cached direct A→B entry keeps geometry and resets all session identity", async ({ page }) => {
  const mock = await setup(page);
  await page.goto(`/agents/${A}`);
  await ready(page, A);
  await animationEnd(page, A, 1);
  await page.evaluate(() => Object.assign(window, { __frameNodes: { root: document.querySelector("[data-agent-frame]"), grid: document.querySelector(".hx-desktop") } }));
  await navigate(page, B);
  await ready(page, B);
  await animationEnd(page, B, 1);
  await navigate(page, A);
  await ready(page, A);
  await animationEnd(page, A, 2);
  if ((page.viewportSize()?.width ?? 0) >= 1100) await page.locator(".hx-inspector-head").getByRole("tab", { name: /^Plan/ }).click();
  await field(page).focus();
  await field(page).evaluate((el: HTMLTextAreaElement) => {
    el.setSelectionRange(2, 8);
    Object.assign(window, { __oldComposer: el, __oldTimeline: document.querySelector(".hx-timeline") });
  });
  await page.keyboard.press("ControlOrMeta+f");
  await page.getByRole("textbox", { name: "Find in this conversation", exact: true }).fill("Alpha");
  await settings(page, A);
  await page.locator(".hx-timeline").evaluate((el) => {
    el.scrollTop = 0;
  });
  await navigate(page, B);
  await ready(page, B);
  await animationEnd(page, B, 2);
  await expect(field(page)).toHaveValue(DRAFTS[B]);
  await expect(page.getByRole("textbox", { name: "Find in this conversation", exact: true })).toHaveCount(0);
  await expect(page.getByRole("dialog", { name: "Agent settings", exact: true })).toHaveCount(0);
  if ((page.viewportSize()?.width ?? 0) >= 1100) await expect(page.locator(".hx-inspector-head").getByRole("tab", { name: /^Changes/ })).toHaveAttribute("aria-selected", "true");
  const identity = await page.evaluate(() => {
    const w = window as unknown as { __frameNodes: { root: Element; grid: Element | null }; __oldComposer: HTMLTextAreaElement; __oldTimeline: Element };
    const input = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Message to the agent"]');
    return {
      root: w.__frameNodes.root === document.querySelector("[data-agent-frame]"),
      grid: w.__frameNodes.grid === document.querySelector(".hx-desktop"),
      oldFieldGone: !w.__oldComposer.isConnected,
      oldTimelineGone: !w.__oldTimeline.isConnected,
      fieldFocused: document.activeElement === input,
      selection: [input?.selectionStart, input?.selectionEnd],
    };
  });
  expect(identity).toMatchObject({ root: true, grid: true, oldFieldGone: true, oldTimelineGone: true, fieldFocused: false });
  expect(identity.selection).not.toEqual([2, 8]);
  await expect.poll(() => page.locator(".hx-timeline").evaluate((el) => el.scrollHeight - el.clientHeight - el.scrollTop)).toBeLessThanOrEqual(5);
  const data = await probe(page);
  expect(new Set(data.observed).size).toBeLessThanOrEqual(1);
  expect(data.animations.filter((event) => event.type === "animationstart")).toHaveLength(4);
  expect(data.animations.filter((event) => event.type === "animationend").every((event) => event.elapsed === 0.18 && event.duration === "0.18s")).toBe(true);
  await field(page).fill("Send only through Frame Beta's store.");
  await page.locator(".hx-dock .cp-send").click();
  await expect.poll(() => mock.commands.filter((entry) => entry.frame.c?.type === "send").map((entry) => [entry.session, entry.frame.c?.text])).toEqual([[B, "Send only through Frame Beta's store."]]);
  await settings(page, B);
  await page.getByRole("combobox", { name: "Subagents at once", exact: true }).selectOption("2");
  await expect.poll(() => mock.commands.filter((entry) => entry.frame.c?.type === "max_subagents").map((entry) => [entry.session, entry.frame.c?.count])).toEqual([[B, 2]]);
});

test("W21-R13 the first loading frame and a held B swap retain geometry without old content or handlers", async ({ page }) => {
  const mock = await setup(page, new Set([A, B]));
  await page.goto(`/agents/${A}`);
  await expect.poll(() => mock.captured.has(A), { timeout: 30_000 }).toBe(true);
  await expect(field(page)).toBeEnabled();
  await expect(field(page)).toHaveValue(DRAFTS[A]);
  await expect.poll(async () => (await probe(page)).entries.length).toBe(1);
  const initial = (await probe(page)).entries[0]!;
  expect(initial).toMatchObject({ session: A, loading: true, header: true, draft: DRAFTS[A], disabled: false, fitted: true });
  expect(initial.height).toBeGreaterThan(0);
  expect(initial.bottom).toBeLessThanOrEqual(initial.viewport + 2);
  expect(initial.pageWidth).toBeLessThanOrEqual(initial.viewportWidth);
  if (initial.viewportWidth >= 1100) {
    expect(initial.grid).toBe(true);
    expect(initial.sideWidth).toBeGreaterThanOrEqual(320);
  } else expect(initial.tabs).toBe(4);
  expect((await probe(page)).animations).toEqual([]);
  const early = "Alpha edit while its snapshot is held.";
  await field(page).fill(early);
  mock.release(A);
  await ready(page, A);
  await expect(field(page)).toHaveValue(early);
  await expect(field(page)).toBeFocused();
  await animationEnd(page, A, 1);
  await page.evaluate(() => Object.assign(window, { __keptFrame: document.querySelector("[data-agent-frame]"), __keptGrid: document.querySelector(".hx-desktop") }));
  await navigate(page, B);
  await expect.poll(() => mock.captured.has(B), { timeout: 30_000 }).toBe(true);
  await expect(field(page)).toBeEnabled();
  await expect(field(page)).toHaveValue(DRAFTS[B]);
  await expect(page.locator(".ag-title")).toHaveText("Agent");
  await expect(page.getByText(/Frame Alpha answer/)).toHaveCount(0);
  await expect(page.getByText(/Frame Beta answer/)).toHaveCount(0);
  expect(
    await page.evaluate(() => {
      const w = window as unknown as { __keptFrame: Element; __keptGrid: Element | null };
      return w.__keptFrame === document.querySelector("[data-agent-frame]") && w.__keptGrid === document.querySelector(".hx-desktop");
    }),
  ).toBe(true);
  expect(mock.commands).toEqual([]);
  mock.release(B);
  await ready(page, B);
  await animationEnd(page, B, 1);
});

test("W21-R13 direct navigation cancels desktop drag and respects reduced motion and short-viewport focus", async ({ page }) => {
  await setup(page);
  await page.goto(`/agents/${A}`);
  await ready(page, A);
  await animationEnd(page, A, 1);
  const desktop = (page.viewportSize()?.width ?? 0) >= 1100;
  if (desktop) {
    const separator = page.getByRole("separator", { name: "Resize side tray", exact: true });
    const columns = await page.locator(".hx-desktop").evaluate((el) => getComputedStyle(el).gridTemplateColumns);
    const box = await separator.boundingBox();
    expect(box).toBeTruthy();
    await page.mouse.move(box!.x + 4, box!.y + 40);
    await page.mouse.down();
    await page.mouse.move(box!.x - 60, box!.y + 40);
    await expect(page.locator(".hx-desktop")).toHaveAttribute("data-resizing", "true");
    await navigate(page, B);
    await ready(page, B);
    await expect(page.locator(".hx-desktop")).not.toHaveAttribute("data-resizing", "true");
    await expect(page.locator(".hx-desktop")).toHaveCSS("grid-template-columns", columns);
    await expect(separator).not.toBeFocused();
    await page.mouse.up();
    await animationEnd(page, B, 1);
  } else await page.getByRole("tab", { name: /^Tools/ }).click();
  await page.emulateMedia({ reducedMotion: "reduce" });
  const before = (await probe(page)).animations.filter((event) => event.type === "animationstart").length;
  const next = desktop ? A : B;
  await navigate(page, next);
  await ready(page, next);
  expect(await page.locator("[data-agent-frame]").evaluate((el) => getComputedStyle(el).animationName)).toBe("none");
  await expect(field(page)).toHaveValue(DRAFTS[next]);
  if (!desktop) await expect(page.getByRole("tab", { name: "Work", exact: true })).toHaveAttribute("aria-selected", "true");
  await field(page).focus();
  await page.setViewportSize({ width: page.viewportSize()!.width, height: 500 });
  await expect(page.locator("html")).toHaveAttribute("data-window", "short");
  await expect(field(page)).toBeFocused();
  await expect(page.locator(".ag-head")).toBeHidden();
  expect((await probe(page)).animations.filter((event) => event.type === "animationstart")).toHaveLength(before);
  await page.emulateMedia({ reducedMotion: "no-preference" });
  await expect(page.locator("[data-agent-frame]")).toHaveCSS("animation-duration", "0.18s");
  await expect.poll(async () => (await probe(page)).animations.filter((event) => event.type === "animationstart").length).toBe(before + 1);
  // Viewport emulation is not a physical phone keyboard/zoom test.
});

test("W21-R02 crossing the desktop breakpoint keeps the same session controller and its pending work", async ({ page }) => {
  const mock = await setup(page);
  await page.route(/\/api\/harness\/files\?/, (route) => route.fulfill({ json: { file_id: "frame-upload" } }));
  await page.setViewportSize({ width: 1440, height: 900 });
  await page.goto(`/agents/${A}`);
  await ready(page, A);
  await animationEnd(page, A, 1);
  await page.locator('.hx-dock input[type="file"]').setInputFiles({ name: "frame-state.txt", mimeType: "text/plain", buffer: Buffer.from("retained attachment") });
  await expect(page.getByRole("button", { name: "Remove frame-state.txt", exact: true })).toBeVisible();
  await page.locator(`.hx-outcome[data-ev="${A}-answer-39"]`).getByRole("button", { name: "Actions on the answer", exact: true }).click();
  await page.getByRole("menuitem", { name: "Reply to", exact: true }).click();
  await expect(page.locator(".cp-reply-text")).toContainText("Frame Alpha answer 39.");
  await page.keyboard.press("ControlOrMeta+f");
  await page.getByRole("textbox", { name: "Find in this conversation", exact: true }).fill("Alpha answer 39");
  await settings(page, A);
  await page.evaluate(() => Object.assign(window, { __breakpointRoot: document.querySelector("[data-agent-frame]"), __controllerSlot: document.querySelector('[data-agent-slot="controller"]') }));
  for (const width of [390, 1440]) {
    await page.setViewportSize({ width, height: 900 });
    await expect(page.getByRole("dialog", { name: "Agent settings", exact: true })).toBeVisible();
    await expect(page.locator(".hx-dock textarea")).toHaveValue(DRAFTS[A]);
    expect(
      await page.evaluate(() => {
        const w = window as unknown as { __breakpointRoot: Element; __controllerSlot: Element };
        return w.__breakpointRoot === document.querySelector("[data-agent-frame]") && w.__controllerSlot === document.querySelector('[data-agent-slot="controller"]');
      }),
    ).toBe(true);
  }
  await page.getByRole("dialog", { name: "Agent settings", exact: true }).getByRole("button", { name: "Close", exact: true }).click();
  await expect(page.getByRole("button", { name: "Remove frame-state.txt", exact: true })).toBeVisible();
  await expect(page.locator(".cp-reply-text")).toContainText("Frame Alpha answer 39.");
  await expect(page.getByRole("textbox", { name: "Find in this conversation", exact: true })).toHaveValue("Alpha answer 39");
  expect((await probe(page)).animations.filter((event) => event.type === "animationstart")).toHaveLength(1);
  await page.getByRole("textbox", { name: "Find in this conversation", exact: true }).press("Escape");
  await field(page).fill("Breakpoint reply stays with Alpha.");
  await page.locator(".hx-dock .cp-send").click();
  await expect.poll(() => mock.commands.filter((entry) => entry.frame.c?.type === "send")).toHaveLength(1);
  const sent = mock.commands.find((entry) => entry.frame.c?.type === "send")!;
  expect(sent.session).toBe(A);
  expect(sent.frame.c).toMatchObject({ text: "Breakpoint reply stays with Alpha.", reply_to: `${A}-answer-39`, attachments: [{ name: "frame-state.txt", file_id: "frame-upload" }] });
  const columns = await page.locator(".hx-desktop").evaluate((el) => getComputedStyle(el).gridTemplateColumns);
  const saved = await page.evaluate(() => localStorage.getItem("kiln.agents.inspector.width"));
  const separator = page.getByRole("separator", { name: "Resize side tray", exact: true });
  const box = await separator.boundingBox();
  expect(box).toBeTruthy();
  await page.mouse.move(box!.x + 4, box!.y + 40);
  await page.mouse.down();
  await page.mouse.move(box!.x - 60, box!.y + 40);
  await expect(page.locator(".hx-desktop")).toHaveAttribute("data-resizing", "true");
  await page.setViewportSize({ width: 390, height: 900 });
  await page.mouse.up();
  await page.setViewportSize({ width: 1440, height: 900 });
  await expect(page.locator(".hx-desktop")).not.toHaveAttribute("data-resizing", "true");
  await expect(page.locator(".hx-desktop")).toHaveCSS("grid-template-columns", columns);
  expect(await page.evaluate(() => localStorage.getItem("kiln.agents.inspector.width"))).toBe(saved);
  // A real pointerup at desktop size still completes and persists a deliberate resize.
  const expectedWidth = Number(await separator.getAttribute("aria-valuenow")) + 64;
  const again = await separator.boundingBox();
  expect(again).toBeTruthy();
  await page.mouse.move(again!.x + 4, again!.y + 40);
  await page.mouse.down();
  await page.mouse.move(again!.x - 60, again!.y + 40);
  await expect(page.locator(".hx-desktop")).toHaveAttribute("data-resizing", "true");
  await page.mouse.up();
  await expect(page.locator(".hx-desktop")).not.toHaveAttribute("data-resizing", "true");
  await expect(separator).toHaveAttribute("aria-valuenow", String(expectedWidth));
  expect(await page.evaluate(() => localStorage.getItem("kiln.agents.inspector.width"))).toBe(String(expectedWidth));
});
