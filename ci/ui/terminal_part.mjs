#!/usr/bin/env node
// Does the page draw the part kiln's server sends ahead of a terminal's replay exactly as it draws the replay
// (W21-R02; docs/wire.md, "A terminal's screen first"; crates/kiln/src/termpart.rs)? A differential check between
// the two, in the page's own terminal (xterm.js, set up as ui/src/components/terminal/xterm-setup.ts sets it up),
// over generated terminal output.
//
//     cargo build --release -p kiln --example term_part
//     node ci/ui/terminal_part.mjs [--cases 500] [--seed 1] [--rows 24] [--cols 80] [--tokens 300] [--profile all]
//                                  [--shrink 3] [--unchecked] [--tool target/release/examples/term_part]
//                                  [--ui ui] [--out ~/.cache/kiln-terminal-part]
//
// Each case is a stream of output from a terminal's start: plain lines as a shell prints them, and around them what
// moves the cursor and changes what a terminal keeps hidden (colours, scroll regions, character sets, modes, the
// alternate screen, a saved cursor), shaped as sessions are and at random. The example writes the replay the server
// would send for it and the part it would send ahead. For each part, xterm.js draws both as the page does: on one
// terminal, left as an earlier attach might have left it, the part behind a full reset and then the replay behind
// another. What it shows after each is compared: every cell of the screen (its text, its width, its colours and
// attributes as they show), whether the cursor shows, where and how it looks. A mismatch is a part the page would
// show otherwise than the replay it's sent ahead of: it exits 1, and `--shrink n` cuts the first n such streams down
// to the pieces that matter.
//
// `--unchecked` compares the parts the server keeps back too (a replay not twice its part's size; a part its own
// emulator draws otherwise): one of those the page draws otherwise is a cut that shouldn't have stood.
//
// Run it over several sizes and seeds after a change to termpart.rs, and when ui/package-lock.json moves xterm.js:
// termpart.rs follows that version's handlers.
import { createRequire } from "node:module";
import { execFileSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import path from "node:path";

const argv = process.argv.slice(2);
const flag = (k, d) => {
  const i = argv.indexOf(k);
  return i < 0 ? d : argv[i + 1];
};
const here = path.dirname(new URL(import.meta.url).pathname);
const ui = path.resolve(flag("--ui", path.join(here, "../../ui")));
const out = path.resolve(flag("--out", path.join(process.env.HOME ?? ".", ".cache/kiln-terminal-part")));
const cases = Number(flag("--cases", "500"));
const seed = Number(flag("--seed", "1"));
const rows = Number(flag("--rows", "24"));
const cols = Number(flag("--cols", "80"));
const tokens = Number(flag("--tokens", "300"));
const profileAsked = flag("--profile", "all");
const shrink = Number(flag("--shrink", "3"));
const TOOL = path.resolve(flag("--tool", path.join(here, "../../target/release/examples/term_part")));
const unchecked = argv.includes("--unchecked");
let said = "";
const pw = createRequire(path.join(ui, "package.json"))("@playwright/test");

// ---------------------------------------------------------------------------------------------- generation
function mulberry32(a) {
  return () => {
    a |= 0;
    a = (a + 0x6d2b79f5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}
const ESC = "\x1b";
const CSI = `${ESC}[`;
function generator(rnd) {
  const int = (a, b) => a + Math.floor(rnd() * (b - a + 1));
  const pick = (xs) => xs[Math.floor(rnd() * xs.length)];
  const words = ["cargo", "build", "ok", "warning:", "error[E0308]", "src/main.rs:12:5", "Compiling", "kiln", "v0.2.0", "done", "~/kiln", "$", "100%", "[=====>   ]"];
  const sgrParams = () => {
    const one = () => {
      const k = rnd();
      if (k < 0.5) return String(pick([0, 1, 2, 3, 4, 7, 22, 23, 24, 27, 39, 49]));
      if (k < 0.7) return String(pick([30, 31, 32, 33, 34, 35, 36, 37, 90, 91, 92, 93, 94, 95, 96, 97]));
      if (k < 0.8) return String(pick([40, 41, 42, 43, 44, 45, 46, 47, 100, 101, 102, 103, 104, 105, 106, 107]));
      if (k < 0.87) return `38;5;${int(0, 255)}`;
      if (k < 0.92) return `48;5;${int(0, 255)}`;
      if (k < 0.96) return `38;2;${int(0, 255)};${int(0, 255)};${int(0, 255)}`;
      return `48;2;${int(0, 255)};${int(0, 255)};${int(0, 255)}`;
    };
    return Array.from({ length: int(1, 3) }, one).join(";");
  };
  const n = (max) => int(1, max);
  const kinds = {
    text: () => Array.from({ length: int(1, 6) }, () => pick(words)).join(" "),
    space: () => " ".repeat(int(1, 12)),
    long: () => pick(["x", "ab", "0123456789"]).repeat(int(Math.floor(cols / 4), cols)),
    wide: () => pick(["名前", "日本語のテキスト", "😀", "👍", "🚀✨", "한국어", "＠ｆｕｌｌ"]),
    combining: () => pick(["é", "äö", "ñ", "क्षि"]),
    ambiguous: () => pick(["§", "±", "…", "→", "─│┌┐└┘", "✓", "✗", "•", "λ", "“quoted”", "×", "°", "█▓▒░", "⠋⠙⠹"]),
    newer: () => pick(["🥱", "🫠", "🪄", "🧑‍💻", "👍🏽", "⚠️", "❤️", "1️⃣", "🇸🇪", "☺", "☺️"]),
    cr: () => "\r",
    lf: () => "\n",
    crlf: () => "\r\n",
    bs: () => "\b".repeat(int(1, 3)),
    tab: () => "\t",
    bell: () => "\x07",
    sgr: () => `${CSI}${sgrParams()}m`,
    sgr0: () => pick([`${CSI}0m`, `${CSI}m`]),
    bolddim: () => pick([`${CSI}1m${CSI}2m`, `${CSI}2m${CSI}1m`, `${CSI}1;2m`, `${CSI}2;1m`]),
    cuu: () => `${CSI}${n(rows)}A`,
    cud: () => `${CSI}${n(rows)}B`,
    cuf: () => `${CSI}${n(cols)}C`,
    cub: () => `${CSI}${n(cols)}D`,
    cnl: () => `${CSI}${n(4)}E`,
    cpl: () => `${CSI}${n(4)}F`,
    cha: () => `${CSI}${n(cols + 2)}G`,
    cup: () => pick([`${CSI}${n(rows + 1)};${n(cols + 2)}H`, `${CSI}H`, `${CSI}${n(rows)}H`]),
    vpa: () => `${CSI}${n(rows + 1)}d`,
    ed: () => pick([`${CSI}J`, `${CSI}0J`, `${CSI}1J`, `${CSI}2J`]),
    el: () => pick([`${CSI}K`, `${CSI}0K`, `${CSI}1K`, `${CSI}2K`]),
    ech: () => `${CSI}${n(cols)}X`,
    dch: () => `${CSI}${n(cols)}P`,
    ich: () => `${CSI}${n(cols)}@`,
    il: () => `${CSI}${n(rows)}L`,
    dl: () => `${CSI}${n(rows)}M`,
    su: () => `${CSI}${n(rows)}S`,
    sd: () => `${CSI}${n(rows)}T`,
    region: () => {
      const top = int(1, rows - 1);
      return `${CSI}${top};${int(top + 1, rows)}r`;
    },
    regionOff: () => `${CSI}r`,
    ri: () => `${ESC}M`,
    save: () => `${ESC}7`,
    restore: () => `${ESC}8`,
    cursorHide: () => `${CSI}?25l`,
    cursorShow: () => `${CSI}?25h`,
    origin: () => pick([`${CSI}?6h`, `${CSI}?6l`]),
    keys: () => pick([`${CSI}?1h`, `${CSI}?1l`, `${ESC}=`, `${ESC}>`, `${CSI}?2004h`, `${CSI}?2004l`, `${CSI}?1000h`, `${CSI}?1006h`, `${CSI}?1000l`]),
    alt: () => `${pick([`${CSI}?1049h`, `${CSI}?47h`])}${CSI}H${CSI}2Jfull screen ${int(1, 99)}\r\nline two`,
    altOff: () => pick([`${CSI}?1049l`, `${CSI}?47l`]),
    title: () => `${ESC}]0;title ${int(1, 9)}\x07`,
    ris: () => `${ESC}c`,
    harmless: () => pick([`${ESC}]7;file://host/tmp${ESC}\\`, `${ESC}]133;A\x07`, `${CSI}?1034h`, `${CSI}>4;2m`, `${CSI}6n`, `${ESC}(B`, `${CSI}3J`, `${CSI}?2026h`, `${CSI}?2026l`, `${CSI}c`, `${CSI}>c`, `${CSI}?u`, `${CSI}>1u`, `${CSI}22;0t`, `${ESC}]11;?\x07`, `${ESC}P+q544e${ESC}\\`, `${CSI}?2031h`, `${CSI}6m`, `${CSI}?25$p`, `${ESC}]104\x07`, `${CSI}?5h`, `${CSI}?3h`]),
    // What the page's terminal keeps hidden.
    sgrx: () => `${CSI}${pick(["5", "8", "9", "21", "53", "4:3", "4:0", "58:5:196", "58;2;10;20;30", "59", "38:5:196", "38:2::10:200:120", "38;5", "48;2;1;2", "25", "28", "29", "55", "38;7;1"])}m`,
    charset: () => pick([`${ESC}(0`, `${ESC}(B`, `${ESC})0`, `${ESC})B`, "\x0e", "\x0f", `${ESC}(A`, `${ESC}n`, `${ESC}%G`, `${CSI}?2h`]),
    insert: () => pick([`${CSI}4h`, `${CSI}4l`]),
    wrap: () => pick([`${CSI}?7l`, `${CSI}?7h`]),
    revwrap: () => pick([`${CSI}?45h`, `${CSI}?45l`]),
    newline: () => pick([`${CSI}20h`, `${CSI}20l`]),
    shape: () => `${CSI}${pick(["", "0", "1", "2", "3", "4", "5", "6", "7", "8"])} q`,
    blinkOpt: () => pick([`${CSI}?12h`, `${CSI}?12l`]),
    link: () => `${ESC}]8;;https://kiln.example/${int(1, 9)}${ESC}\\link${ESC}]8;;${ESC}\\`,
    linkOpen: () => `${ESC}]8;id=a;https://kiln.example${ESC}\\`,
    linkClose: () => `${ESC}]8;;${ESC}\\`,
    tabs: () => pick([`${ESC}H`, `${CSI}g`, `${CSI}3g`]),
    rep: () => `${pick(["x", "名", "-"])}${CSI}${n(cols)}b`,
    softReset: () => `${CSI}!p`,
    palette: () => pick([`${ESC}]4;1;rgb:ff/00/00\x07`, `${ESC}]11;#102030\x07`, `${ESC}]10;?\x07`]),
    c1: () => pick(["\u0085", "\u009b31m", "\u0084", "\u008d"]),
    saveAnsi: () => `${CSI}s`,
    restoreAnsi: () => `${CSI}u`,
    save1048: () => `${CSI}?1048h`,
    restore1048: () => `${CSI}?1048l`,
    alt1047: () => `${CSI}?1047h${CSI}H${CSI}2Jfull ${int(1, 99)}`,
    alt1047Off: () => `${CSI}?1047l`,
    align: () => `${ESC}#8`,
    columns: () => pick([`${CSI}${n(4)}'}`, `${CSI}${n(4)}'~`, `${CSI}${n(4)} @`, `${CSI}${n(4)} A`]),
    hpa: () => pick([`${CSI}${n(cols)}\``, `${CSI}${n(8)}a`, `${CSI}${n(3)}I`, `${CSI}${n(3)}Z`]),
    vpr: () => pick([`${CSI}${n(rows)}e`, `${CSI}${n(rows)};${n(cols)}f`]),
    index: () => pick([`${ESC}D`, `${ESC}E`, "\x0b", "\x0c"]),
    selective: () => pick([`${CSI}?K`, `${CSI}?1K`, `${CSI}?J`, `${CSI}?2J`]),
    protect: () => pick([`${CSI}1"q`, `${CSI}0"q`]),
    // What two parsers might read differently: controls inside a sequence, sequences given up or cut short,
    // strings with line feeds in them, parameters out of shape, bytes that aren't UTF-8.
    inside: () => pick([`${CSI}3\n1m`, `${CSI}\r\n0m`, `${CSI}3\r1m`, `${CSI}4\n`, `${ESC}\n(0`, `${CSI}?25\nl`, `${CSI}1;\n31m`]),
    givenUp: () => pick([`${CSI}3\x18`, `${CSI}3\x1a1m`, `${CSI}3${CSI}0m`, `${ESC}${ESC}[0m`, `${CSI}?1;?2h`, `${CSI}1?h`, `${CSI}1 \x18`, `${ESC}(`]),
    shapes: () => pick([`${CSI};;5m`, `${CSI}1:2:3m`, `${CSI}38;5;m`, `${CSI}38;2;1;2m`, `${CSI}${"1;".repeat(40)}m`, `${CSI}99999999999m`, `${CSI}0000031m`, `${CSI}31;m`, `${CSI}:31m`, `${CSI}4:m`, `${CSI}  q`, `${CSI}1$$p`]),
    strings: () => pick([`${ESC}]0;a\nb\x07`, `${ESC}]0;title`, `${ESC}_an apc\nstring${ESC}\\`, `${ESC}^a pm${ESC}\\`, `${ESC}Xan sos${ESC}\\`, `${ESC}P1$r0m${ESC}\\`, `${ESC}Pq#0;2;0;0;0\n#0~~${ESC}\\`, `${ESC}]8;;http://a\x07`, `${ESC}]8;;\x07`, `${ESC}]0;t\x9c`, `${ESC}]1337;File=inline=1:AAAA\x07`, `${ESC}]+8;;\x07`, `${ESC}] 8;;http://a\x07`, `${ESC}]08;;http://b\x07`, `${ESC}];8;;http://c\x07`]),
    raw: () => pick([[0x80], [0x9b, 0x33, 0x31, 0x6d], [0xc3], [0xe2, 0x82], [0xff], [0xc0, 0xaf], [0x84], [0x85], [0x8d], [0x9d, 0x30, 0x3b, 0x74, 0x07], [0x7f], [0x00], [0xed, 0xa0, 0x80]]),
    // Past ASCII inside a sequence, and a C1 control as a character inside a string.
    beyond: () => pick([`${CSI}3é1m`, `${CSI}é0m`, `${ESC}é(0`, `${CSI}\u009b0m`, `${CSI}3\u00851m`, `${ESC}]0;t\u009c`, `${ESC}]0;a\u0085b\x07`, `${ESC}P1$r\u009c`, `${ESC}_app\u009dtitle\x07`, `${ESC}]0;名前 é\x07`, `${CSI}名m`]),
    // Parameters with others riding on them, and left empty.
    riding: () => `${CSI}${pick(["1:2:3", "4:", "4:0", "4::", "0:1", "22:1", "31:1", "39:5", "7:0;1:0", "38:2::1:2:3", "38:5:", "38:", "48::5:1", "58:2:0:1:2:3", "24:3", "9:1;29:0", "4:3;4:"])}m`,
  };
  // How often each kind comes, by what the output is like.
  const base = { text: 10, space: 2, cr: 1, lf: 3, crlf: 6, sgr: 6, sgr0: 3 };
  const profiles = {
    lines: { ...base, long: 2, tab: 1, bs: 1, bell: 0.3, title: 0.3, bolddim: 0, harmless: 1 },
    intensity: { ...base, bolddim: 3 },
    unicode: { ...base, wide: 5, combining: 2, ambiguous: 4, newer: 4, long: 2, cha: 1, cup: 1, cuf: 1, tab: 1 },
    cursor: { ...base, cuu: 2, cud: 2, cuf: 2, cub: 2, cnl: 1, cpl: 1, cha: 2, cup: 3, vpa: 1, ed: 1, el: 3, ech: 1, dch: 1, ich: 1, bs: 2, tab: 2, long: 2, save: 1, restore: 1, cursorHide: 0.5, cursorShow: 0.5 },
    scroll: { ...base, il: 2, dl: 2, su: 2, sd: 2, region: 2, regionOff: 1, ri: 2, cup: 2, cuu: 1, lf: 6, origin: 1, long: 1 },
    progress: { text: 6, cr: 6, el: 5, cuu: 3, crlf: 3, sgr: 3, sgr0: 2, ambiguous: 2, cursorHide: 0.5, cursorShow: 0.5 },
    modes: { ...base, keys: 3, alt: 1, altOff: 1.2, title: 1, cursorHide: 1, cursorShow: 1, ris: 0.2, harmless: 3, save: 1, restore: 1 },
  };
  profiles.mixed = Object.fromEntries(Object.keys(kinds).map((k) => [k, k === "ris" || k === "softReset" || k === "palette" ? 0.1 : k === "text" || k === "crlf" ? 8 : 1]));
  // Within a line: nothing that moves the cursor off it.
  const within = { newline: 0.15, text: 10, space: 2, sgr: 6, sgr0: 4, sgrx: 1.5, bolddim: 0.5, long: 1, wide: 1, combining: 0.5, ambiguous: 1, newer: 1, tab: 1, bs: 1, cr: 0.7, bell: 0.3, title: 0.3, harmless: 1.5, el: 1.5, ech: 0.7, dch: 0.7, ich: 0.7, cuf: 1, cub: 1, cha: 1, hpa: 0.7, rep: 0.5, link: 1, cursorHide: 0.3, cursorShow: 0.3, keys: 1, shape: 0.5, blinkOpt: 0.3, lf: 0.3, save: 0.3 };
  /** A piece of `kind`: its text, or its bytes where they aren't text. */
  const piece = (kind) => {
    const made = kinds[kind]();
    return typeof made === "string" ? { kind, text: made } : { kind, bytes: Buffer.from(made) };
  };
  const drawer = (weights) => {
    const table = Object.entries(weights).filter(([, w]) => w > 0);
    const total = table.reduce((s, [, w]) => s + w, 0);
    return () => {
      let at = rnd() * total;
      for (const [k, w] of table) {
        at -= w;
        if (at <= 0) return piece(k);
      }
      return piece("text");
    };
  };
  // A line with what parsers might read differently in it.
  const garbled = { ...within, inside: 2, givenUp: 2, shapes: 2, strings: 2, raw: 2, c1: 1, beyond: 2, riding: 3 };
  const draw = Object.fromEntries(Object.entries({ ...profiles, within, garbled }).map(([name, weights]) => [name, drawer(weights)]));
  const some = (name, count) => Array.from({ length: count }, draw[name]);
  /** `count` lines of a shell's output, each ended as a terminal's are; its pieces drawn as `of` says. */
  const plain = (count, of = "within") => Array.from({ length: count }, () => [...some(of, int(0, 5)), { kind: "crlf", text: "\r\n" }]).flat();
  // Shaped as a session is: plain output around what moves the cursor and changes what's hidden.
  const shaped = {
    after: () => [...plain(2 * rows + int(0, rows)), ...some("mixed", int(3, 40))],
    between: () => [...some("mixed", int(3, 40)), ...plain(2 * rows + int(0, 2 * rows)), ...some("mixed", int(0, 30))],
    tight: () => [...some("mixed", int(3, 20)), ...plain(int(rows, 2 * rows + 2)), ...some("mixed", int(0, 10))],
    session: () => Array.from({ length: int(3, 6) }, () => [...plain(int(1, 3 * rows)), ...some("mixed", int(0, 12))]).flat(),
    full: () => [...plain(2 * rows + int(0, rows)), ...some("modes", int(2, 10)), piece("alt"), ...some("cursor", int(3, 30)), piece("altOff"), ...plain(int(0, 3 * rows))],
    garbled: () => plain(3 * rows + int(0, 2 * rows), "garbled"),
    garbledAfter: () => [...plain(2 * rows + int(0, rows)), ...plain(int(1, 2 * rows), "garbled")],
  };
  const names = [...Object.keys(profiles), ...Object.keys(shaped), ...Object.keys(shaped)];
  return (count, asked) => {
    const profile = asked === "all" ? pick(names) : asked;
    return { profile, tokens: shaped[profile] ? shaped[profile]() : some(profile, count) };
  };
}

// ---------------------------------------------------------------------------------------------- checking
const browser = await pw.chromium.launch({ channel: process.env.E2E_CHANNEL ?? "chrome", headless: true });
const page = await browser.newPage();
await page.setContent("<!doctype html><title>term-fuzz</title>");
await page.addScriptTag({ path: path.join(ui, "node_modules/@xterm/xterm/lib/xterm.js") });
await page.addScriptTag({ path: path.join(ui, "node_modules/@xterm/addon-unicode11/lib/addon-unicode11.js") });

/**
 * Each case drawn as the page draws it, on one terminal: whatever it showed before (`before`: what an earlier attach
 * left it with), then the part behind a full reset, then the replay behind another. What the terminal shows after the
 * part and after the replay are compared: null where they are the same, else what differs first.
 */
const compare = (cases) =>
  page.evaluate(
    async ([cases, rows, cols]) => {
      const bytes = (b64) => Uint8Array.from(atob(b64), (c) => c.charCodeAt(0));
      /** `data` behind a full reset, as the page writes both (`fromFresh`, ui/src/views/terminal/session.tsx). */
      const fromFresh = (data) => {
        const all = new Uint8Array(data.length + 2);
        all.set([0x1b, 0x63]);
        all.set(data, 2);
        return all;
      };
      const shown = (t) => {
        const b = t.buffer.active;
        const cell = b.getNullCell();
        const lines = [];
        for (let y = 0; y < rows; y++) {
          const line = b.getLine(b.baseY + y);
          const cells = [];
          for (let x = 0; x < cols; x++) {
            line.getCell(x, cell);
            const text = cell.getChars() || " ";
            const fg = `${cell.getFgColorMode()}:${cell.getFgColor()}`;
            const bg = `${cell.getBgColorMode()}:${cell.getBgColor()}`;
            const inverse = cell.isInverse() !== 0;
            const lined = (cell.isUnderline() ? "u" : "") + (cell.isStrikethrough() ? "s" : "") + (cell.isOverline() ? "o" : "");
            if (cell.getWidth() === 0) cells.push("<");
            else if (text === " ")
              // A blank shows its background (its foreground, inverted) and any line through or under it.
              cells.push(`_|${inverse ? fg : bg}|${lined}${lined ? `|${fg}|${cell.isDim() ? "d" : ""}` : ""}`);
            else cells.push(`${text}|${cell.getWidth()}|${fg}|${bg}|${cell.isBold() ? "B" : ""}${cell.isDim() ? "d" : ""}${cell.isItalic() ? "i" : ""}${inverse ? "v" : ""}${cell.isBlink() ? "k" : ""}${cell.isInvisible() ? "h" : ""}${lined}`);
          }
          lines.push(cells);
        }
        const core = t._core.coreService;
        // The cursor's look: whether it shows, its shape and its blink, as a sequence set them and as the terminal's own.
        const look = `${core.decPrivateModes.cursorStyle ?? "-"}/${core.decPrivateModes.cursorBlink ?? "-"}/${t.options.cursorBlink}`;
        return { lines, cursor: `${b.cursorX},${b.cursorY}`, hidden: core.isCursorHidden, look, type: b.type };
      };
      const results = [];
      for (const [replay, part, before] of cases) {
        // As the page's own terminal is made (ui/src/components/terminal/xterm-setup.ts), where it bears on what a cell holds.
        const t = new Terminal({ allowProposedApi: true, cols, rows, scrollback: 10_000, cursorBlink: true, cursorStyle: "bar" });
        t.loadAddon(new Unicode11Addon.Unicode11Addon());
        t.unicode.activeVersion = "11";
        const draw = (data) => new Promise((done) => t.write(data, done));
        await draw(bytes(before));
        await draw(fromFresh(bytes(part)));
        const B = shown(t);
        await draw(fromFresh(bytes(replay)));
        const A = shown(t);
        t.dispose();
        let differs = null;
        if (A.type !== B.type) differs = { what: "buffer", replay: A.type, part: B.type };
        for (let y = 0; y < rows && !differs; y++)
          for (let x = 0; x < cols && !differs; x++) if (A.lines[y][x] !== B.lines[y][x]) differs = { what: "cell", at: `${x},${y}`, replay: A.lines[y][x], part: B.lines[y][x], row: [A.lines[y].map((c) => c.split("|")[0]).join(""), B.lines[y].map((c) => c.split("|")[0]).join("")] };
        if (!differs && A.hidden !== B.hidden) differs = { what: "cursor shown", replay: !A.hidden, part: !B.hidden };
        // Where the cursor is, and how it looks, show only while it does.
        if (!differs && !A.hidden && A.cursor !== B.cursor) differs = { what: "cursor", replay: A.cursor, part: B.cursor };
        if (!differs && !A.hidden && A.look !== B.look) differs = { what: "cursor's look", replay: A.look, part: B.look };
        results.push(differs);
      }
      return results;
    },
    [cases, rows, cols],
  );

/** What a terminal may have been left with by an attach before this one: the page keeps some of it through a full reset. */
const BEFORE = ["", "", "\x1b[?25l", "\x1b[?12l", "\x1b[20h", "\x1b[?25l\x1b[?12h\x1b[4 q\x1b[31m\x1b(0\x1b[2;4r", "\x1b]11;#102030\x07", "left over\r\nfrom before\r\n\x1b[?1049h\x1b[?25l"];

/** For each stream (its pieces): null where no part goes to be compared, else what differs between the two drawings (null within: nothing) and what kept the part back, if anything did. */
let batches = 0;
async function check(streams) {
  const dir = path.join(out, `batch-${batches++}`);
  rmSync(dir, { recursive: true, force: true });
  mkdirSync(dir, { recursive: true });
  const name = (i) => String(i).padStart(5, "0");
  streams.forEach((tokens, i) => writeFileSync(path.join(dir, `${name(i)}.in`), Buffer.concat(tokens.map((t) => t.bytes ?? Buffer.from(t.text, "utf8")))));
  said = execFileSync(TOOL, [String(rows), String(cols), dir, ...(unchecked ? ["--unchecked"] : [])]).toString().trim();
  const sent = streams.map((_, i) => existsSync(path.join(dir, `${name(i)}.part`)));
  const b64 = (i, of) => readFileSync(path.join(dir, `${name(i)}.${of}`)).toString("base64");
  // What the terminal was left with before: one of a few, by the stream's bytes.
  const before = (i) => Buffer.from(BEFORE[readFileSync(path.join(dir, `${name(i)}.in`)).length % BEFORE.length], "utf8").toString("base64");
  const pairs = streams.flatMap((_, i) => (sent[i] ? [[b64(i, "replay"), b64(i, "part"), before(i)]] : []));
  const compared = pairs.length ? await compare(pairs) : [];
  let at = 0;
  const kept = (i) => (existsSync(path.join(dir, `${name(i)}.kept`)) ? readFileSync(path.join(dir, `${name(i)}.kept`), "utf8") : null);
  const results = streams.map((_, i) => (sent[i] ? { differs: compared[at++], kept: kept(i) } : null));
  rmSync(dir, { recursive: true, force: true });
  return results;
}

mkdirSync(out, { recursive: true });
const make = generator(mulberry32(seed));
const all = Array.from({ length: cases }, () => make(tokens, profileAsked));
const results = await check(all.map((c) => c.tokens));
const tool = said;
const sent = results.filter((r) => r && !r.kept).length;
const wrong = results.map((r, i) => ({ r, i })).filter(({ r }) => r?.differs && !r.kept);
if (unchecked) {
  const by = {};
  for (const r of results.filter((r) => r?.kept)) {
    const k = (by[r.kept] ??= { n: 0, wrong: 0 });
    k.n++;
    if (r.differs) k.wrong++;
  }
  console.log(`  kept back, and compared all the same: ${Object.entries(by).map(([k, v]) => `by ${k}: ${v.n}, of which the page draws ${v.wrong} otherwise`).join("; ") || "none"}`);
}
console.log(`${cases} streams on ${cols}x${rows} (seed ${seed}, profile ${profileAsked}): a part would be sent ahead of ${sent} (${tool}); ${wrong.length} of those the page draws otherwise than the replay`);
const byProfile = {};
all.forEach((c, i) => {
  const p = (byProfile[c.profile] ??= { cases: 0, sent: 0, wrong: 0 });
  p.cases++;
  if (results[i] && !results[i].kept) p.sent++;
  if (results[i]?.differs && !results[i].kept) p.wrong++;
});
console.log(Object.entries(byProfile).map(([p, v]) => `  ${p}: ${v.wrong} wrong of ${v.sent} sent (${v.cases} streams)`).join("\n"));
// Which kinds of piece go with a mismatch: how much oftener one is in a mismatching stream than in a sent one.
if (wrong.length) {
  const [inWrong, inSent] = [{}, {}];
  all.forEach((c, i) => {
    if (!results[i] || results[i].kept) return;
    for (const k of new Set(c.tokens.map((t) => t.kind))) {
      inSent[k] = (inSent[k] ?? 0) + 1;
      if (results[i].differs) inWrong[k] = (inWrong[k] ?? 0) + 1;
    }
  });
  const lift = Object.keys(inWrong)
    .map((k) => ({ k, share: inWrong[k] / wrong.length, lift: inWrong[k] / wrong.length / (inSent[k] / sent) }))
    .sort((a, b) => b.lift - a.lift);
  console.log(`  pieces oftenest in the mismatching: ${lift.slice(0, 8).map((l) => `${l.k} (in ${(l.share * 100).toFixed(0)}%, ${l.lift.toFixed(1)}x)`).join(", ")}`);
  const whats = {};
  for (const { r } of wrong) whats[r.differs.what] = (whats[r.differs.what] ?? 0) + 1;
  console.log(`  what differs first: ${Object.entries(whats).map(([k, n]) => `${k} ${n}`).join(", ")}`);
}

// Each of the first few cut down to the pieces that matter (delta debugging, a batch a round).
const show = (t) => JSON.stringify(t.bytes ? t.bytes.toString("latin1") : t.text).slice(1, -1).replaceAll("\\u001b", "ESC ");
const toShrink = unchecked ? results.map((r, i) => ({ r, i })).filter(({ r }) => r?.differs) : wrong;
for (const { i } of toShrink.slice(0, shrink)) {
  let T = all[i].tokens;
  let n = 2;
  while (T.length >= 2) {
    const size = Math.ceil(T.length / n);
    const without = Array.from({ length: Math.ceil(T.length / size) }, (_, k) => [...T.slice(0, k * size), ...T.slice((k + 1) * size)]);
    const got = await check(without);
    const still = got.findIndex((r) => r?.differs && (unchecked || !r.kept));
    if (still >= 0) {
      T = without[still];
      n = Math.max(n - 1, 2);
    } else if (size > 1) n = Math.min(T.length, n * 2);
    else break;
  }
  const [last] = await check([T]);
  console.log(`\n  stream ${i} (${all[i].profile}${results[i].kept ? `, kept back by ${results[i].kept}` : ""}), cut to ${T.length} pieces: ${T.map(show).join(" · ")}`);
  console.log(`    ${JSON.stringify(last?.differs)}`);
}
await browser.close();
process.exit(wrong.length ? 1 : 0);
