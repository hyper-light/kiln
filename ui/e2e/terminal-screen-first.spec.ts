/**
 * W21-R02: a terminal with a long history shows its screen as soon as that is in. The server sends the newest part of
 * the replay ahead of it to a view that asks (docs/wire.md, "A terminal's screen first"): the last screen of lines,
 * which draw its screen. The replay then draws the same screen with its scrollback above it: nothing on the screen
 * moves as it comes, and no frame shows the terminal empty or part way through its history. The view takes the
 * session's size for itself only once the replay is drawn, and answers what the part asks of it only once: as the
 * replay asks it.
 */
import { expect, test, type Page } from "@playwright/test";

const SESSION = "ts-long";
/** The session has said this many lines. */
const LINES = 600;
const line = (n: number) => `line ${String(n).padStart(4, "0")} of the build: ok`;

interface Size {
  cols: number;
  rows: number;
}

const info = (size: Size) => ({
  id: SESSION,
  target: "local",
  command: null,
  title: "",
  created_at: 1_794_000_000,
  attached: 0,
  cols: size.cols,
  rows: size.rows,
  idle_seconds: 0,
  exited: false,
  exit_code: null,
  error: null,
});

/** The session's replay: all it has said, ending at its prompt; then `asks`, what its program last asked of the terminal. */
const replay = (asks = "") => `\x1b[0m${Array.from({ length: LINES }, (_, i) => line(i + 1)).join("\r\n")}\r\n$ ${asks}`;
/**
 * The part of it the server sends ahead to a terminal of `rows` rows (`kiln::termpart`): the cursor put on the last row,
 * then the replay's last lines, as many as the screen has rows, and what follows them.
 */
const part = (rows: number, asks = "") => `\x1b[0m\x1b[${rows};1H${Array.from({ length: rows }, (_, i) => line(LINES - rows + i + 1)).join("\r\n")}\r\n$ ${asks}`;

/**
 * The terminal page with one session listed, and its socket played here as the server plays it: the `session` frame,
 * then (`ahead`) the part that draws its screen. `open` sets the session's size, whether a part goes ahead and what its
 * program last asked of the terminal, and loads the page. The rest send the replay and output when the test says, and
 * say what the view asked for and sent.
 */
async function terminal(page: Page) {
  type Up = { type?: string; cols?: number; rows?: number };
  const is = { size: { cols: 20, rows: 6 }, ahead: false, asks: "" };
  let sent: Up[] = [];
  let typed: string[] = [];
  let asked = "";
  let down: ((data: string | Buffer) => void) | null = null;
  await page.route("**/api/terminal/sessions", (route) => route.fulfill({ json: [info(is.size)] }));
  await page.routeWebSocket(
    (url) => url.pathname === "/api/terminal/ws",
    (ws) => {
      asked = ws.url();
      const mine: Up[] = [];
      const bytes: string[] = [];
      sent = mine;
      typed = bytes;
      down = (data) => ws.send(data);
      ws.onMessage((message) => {
        // Text is the view's frames; binary is what goes to the shell as typed.
        if (typeof message === "string") mine.push(JSON.parse(message) as Up);
        else bytes.push(message.toString("latin1"));
      });
      ws.send(JSON.stringify({ type: "session", session: { ...info(is.size), attached: 1 }, ...(is.ahead ? { screen: true } : {}) }));
      if (is.ahead) ws.send(Buffer.from(part(is.size.rows, is.asks)));
    },
  );
  const say = (data: string | Buffer) => {
    if (!down) throw new Error("The terminal's socket has not opened");
    down(data);
  };
  const resizes = () => sent.filter((frame) => frame.type === "resize").map((frame) => ({ cols: frame.cols, rows: frame.rows }));
  const open = async (size: Size, ahead: boolean, asks = "") => {
    Object.assign(is, { size, ahead, asks });
    asked = "";
    down = null;
    await page.goto("/terminal");
    await expect.poll(() => asked, { timeout: 30_000 }).toContain("first=screen");
  };
  return {
    open,
    resizes,
    replay: () => say(Buffer.from(replay(is.asks))),
    /** What the view sent to the shell as typed. */
    typed: () => [...typed],
    output: (text: string) => say(Buffer.from(text)),
    /** The size this view gives a session it's in use on: what it sends a session of another size, once that is drawn. */
    ownSize: async (): Promise<Size> => {
      await open({ cols: 20, rows: 6 }, false);
      await expect.poll(() => resizes().length, { timeout: 30_000 }).toBeGreaterThan(0);
      const size = resizes().at(-1);
      if (!size?.cols || !size.rows) throw new Error("The view sent no size");
      return { cols: size.cols, rows: size.rows };
    },
  };
}

const ROWS = ".term-host:not([aria-hidden='true']) .xterm-rows";
const painted = (page: Page) => page.evaluate(() => new Promise<void>((resolve) => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
/** What the terminal shows: its rows' text, top to bottom. */
const shown = (page: Page) => page.locator(`${ROWS} > div`).evaluateAll((rows) => rows.map((row) => (row.textContent ?? "").trimEnd()));
/**
 * Keeps what the terminal shows at each frame from here on (`window.__screens`), read as the frame is about to be
 * painted: after everything that draws in it has (the terminal draws its rows in an animation-frame callback of its
 * own, having emptied them as it read what came). A resize observer's callback runs then, so each frame resizes a
 * hidden element it watches.
 */
const eachFrame = (page: Page) =>
  page.evaluate((selector) => {
    const seen = ((window as unknown as { __screens: string[] }).__screens = [] as string[]);
    const probe = document.createElement("div");
    probe.style.cssText = "position:fixed;left:-9px;top:0;width:1px;height:1px;visibility:hidden;pointer-events:none";
    document.body.append(probe);
    new ResizeObserver(() => seen.push(Array.from(document.querySelectorAll(`${selector} > div`), (row) => (row.textContent ?? "").trimEnd()).join("\n"))).observe(probe);
    let tall = false;
    const tick = () => {
      tall = !tall;
      probe.style.height = tall ? "2px" : "1px";
      requestAnimationFrame(tick);
    };
    requestAnimationFrame(tick);
  }, ROWS);
/** The screen of a session of `size` that has said its `LINES` lines: the last of them, above its prompt. */
const lastScreen = (size: Size) => [...Array.from({ length: size.rows - 1 }, (_, i) => line(LINES - (size.rows - 1) + i + 1)), "$"];

test.beforeEach(() => test.skip(!!process.env.E2E_BASE_URL, "a controlled terminal's socket"));

test("W21-R02 a terminal shows its screen as soon as that is in, and its replay joins above it with nothing on the screen moving", async ({ page }) => {
  const errors: string[] = [];
  page.on("pageerror", (error) => errors.push(error.message));
  const session = await terminal(page);
  // A session of this view's own size: nothing resizes it.
  const size = await session.ownSize();
  await session.open(size, true);
  // The part alone: the session's last lines and its prompt.
  const rows = page.locator(ROWS);
  await expect(rows).toContainText(line(LINES), { timeout: 30_000 });
  await painted(page);
  const first = await shown(page);
  expect(first).toEqual(lastScreen(size));
  // What the terminal shows, at every frame from here until the replay is in and the session has said something more.
  await eachFrame(page);
  await painted(page);
  session.replay();
  session.output("done\r\n$ ");
  await expect(rows).toContainText("$ done");
  await painted(page);
  const screens = await page.evaluate(() => (window as unknown as { __screens: string[] }).__screens);
  const distinct = screens.filter((screen, i) => screen !== screens[i - 1]);
  // The screen as it was, at every frame, and then the line the session said after it: never empty, never part way.
  expect(screens.length).toBeGreaterThan(3);
  expect(distinct).toEqual([first.join("\n"), [...first.slice(1, -1), "$ done", "$"].join("\n")]);
  // The replay is in: what the session said before its screen is above it.
  await page.keyboard.press("Shift+PageUp");
  await expect(rows).toContainText(line(LINES - size.rows - 2));
  expect(session.resizes()).toEqual([]);
  expect(errors).toEqual([]);
});

test("W21-R02 a view takes a session's size only once the replay behind its screen is drawn", async ({ page }) => {
  const session = await terminal(page);
  const own = await session.ownSize();
  // A session another device sized: narrower and shorter than this view makes one.
  const size = { cols: own.cols - 7, rows: own.rows - 3 };
  await session.open(size, true);
  const rows = page.locator(ROWS);
  await expect(rows).toContainText(line(LINES), { timeout: 30_000 });
  // The screen shows at the session's size, and stays at it: the view asks for no other while the replay is on its way.
  await painted(page);
  await painted(page);
  expect(await shown(page)).toEqual(lastScreen(size));
  expect(session.resizes()).toEqual([]);
  session.replay();
  // The replay drawn, the view is the one in use: it takes the size.
  await expect.poll(() => session.resizes()).toEqual([own]);
});

test("W21-R02 what a part asks of the terminal is answered once, as its replay asks it", async ({ page }) => {
  const session = await terminal(page);
  const own = await session.ownSize();
  // The session's program last asked where the cursor is: the part holds that, and the replay holds it again.
  const asks = "\x1b[6n";
  const answer = `\x1b[${own.rows};3R`;
  await session.open(own, true, asks);
  const rows = page.locator(ROWS);
  await expect(rows).toContainText(line(LINES), { timeout: 30_000 });
  await painted(page);
  await painted(page);
  expect(session.typed(), "nothing is answered out of the part").toEqual([]);
  session.replay();
  session.output("done\r\n$ ");
  await expect(rows).toContainText("$ done");
  await painted(page);
  expect(session.typed()).toEqual([answer]);
  // With no part ahead, the replay's is answered as it always was.
  await session.open(own, false, asks);
  session.replay();
  session.output("done\r\n$ ");
  await expect(rows).toContainText("$ done", { timeout: 30_000 });
  await painted(page);
  expect(session.typed()).toEqual([answer]);
});

test("W21-R02 a session with no part sent ahead is drawn from its replay, as before", async ({ page }) => {
  const session = await terminal(page);
  const own = await session.ownSize();
  await session.open(own, false);
  session.replay();
  const rows = page.locator(ROWS);
  await expect(rows).toContainText(line(LINES), { timeout: 30_000 });
  await painted(page);
  expect(await shown(page)).toEqual(lastScreen(own));
  await page.keyboard.press("Shift+PageUp");
  await expect(rows).toContainText(line(LINES - own.rows - 2));
});
