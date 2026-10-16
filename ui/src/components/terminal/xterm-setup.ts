import { Terminal, type ITerminalOptions, type ITheme } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { WebglAddon } from "@xterm/addon-webgl";
import { WebLinksAddon } from "@xterm/addon-web-links";
import { Unicode11Addon } from "@xterm/addon-unicode11";
import { SearchAddon } from "@xterm/addon-search";
import "@xterm/xterm/css/xterm.css";

/** Pastel ANSI palette harmonised with the status colours and prism stops. */
export const KILN_THEME: ITheme = {
  background: "#08090a",
  foreground: "#d9dbe0",
  cursor: "#d8d0f2",
  cursorAccent: "#08090a",
  selectionBackground: "#aca1d566",
  selectionInactiveBackground: "#aca1d533",
  scrollbarSliderBackground: "#ffffff14",
  scrollbarSliderHoverBackground: "#ffffff24",
  scrollbarSliderActiveBackground: "#ffffff30",
  black: "#1c1e23",
  red: "#d79fa4",
  green: "#9fcab3",
  yellow: "#d4bd95",
  blue: "#9dbbe6",
  magenta: "#bba7dc",
  cyan: "#9fcfcb",
  white: "#c9cbd1",
  brightBlack: "#62666f",
  brightRed: "#e8bcc0",
  brightGreen: "#bfe0cd",
  brightYellow: "#e6d3b0",
  brightBlue: "#bfd4f3",
  brightMagenta: "#d1c2ea",
  brightCyan: "#c2e3df",
  brightWhite: "#f2f2f3",
};

export const TERM_FONT = '"Geist Mono Variable", ui-monospace, "SFMono-Regular", "JetBrains Mono", Menlo, Consolas, monospace';

let fontReady: Promise<unknown> | null = null;
/** Wait for Geist Mono so xterm measures cells with the real glyph metrics. */
export function ensureFont(): Promise<unknown> {
  if (!fontReady) {
    fontReady = Promise.race([
      Promise.all([
        document.fonts.load(`13px "Geist Mono Variable"`),
        document.fonts.load(`bold 13px "Geist Mono Variable"`),
      ]),
      new Promise((r) => setTimeout(r, 1500)),
    ]).catch(() => undefined);
  }
  return fontReady;
}

export interface KilnTerminal {
  term: Terminal;
  fit: FitAddon;
  search: SearchAddon;
  /** "webgl" or "dom" — whichever renderer actually loaded */
  renderer: () => "webgl" | "dom";
  dispose: () => void;
}

/**
 * @param webgl  use the WebGL renderer (PTY sessions). Read-only log views pass false:
 *               browsers cap live WebGL contexts (~16), and the DOM renderer is plenty for logs.
 */
export function createTerminal(host: HTMLElement, opts: ITerminalOptions = {}, useWebgl = true): KilnTerminal {
  const term = new Terminal({
    allowProposedApi: true,
    fontFamily: TERM_FONT,
    fontSize: 13,
    lineHeight: 1.12,
    letterSpacing: 0,
    fontWeight: 400,
    fontWeightBold: 600,
    cursorBlink: true,
    cursorStyle: "bar",
    cursorWidth: 2,
    cursorInactiveStyle: "outline",
    scrollback: 10_000,
    macOptionIsMeta: true,
    macOptionClickForcesSelection: true,
    rightClickSelectsWord: true,
    drawBoldTextInBrightColors: false,
    minimumContrastRatio: 1,
    smoothScrollDuration: 0,
    theme: KILN_THEME,
    ...opts,
  });

  const fit = new FitAddon();
  const search = new SearchAddon();
  const unicode = new Unicode11Addon();
  term.loadAddon(fit);
  term.loadAddon(search);
  term.loadAddon(unicode);
  term.loadAddon(
    new WebLinksAddon((event, uri) => {
      event.preventDefault();
      window.open(uri, "_blank", "noopener,noreferrer");
    }),
  );
  term.unicode.activeVersion = "11";
  term.open(host);

  // The DOM renderer by default: the WebGL one draws nothing at all on some phones and GPUs, which
  // reads as a dead terminal. WebGL is opt-in (localStorage kiln.term.renderer = "webgl") and still
  // falls back to the DOM renderer on failure or context loss.
  let renderer: "webgl" | "dom" = "dom";
  let webgl: WebglAddon | null = null;
  let wantWebgl = false;
  try {
    wantWebgl = localStorage.getItem("kiln.term.renderer") === '"webgl"';
  } catch {
    /* storage unavailable */
  }
  if (useWebgl && wantWebgl) {
    try {
      const addon = new WebglAddon();
      addon.onContextLoss(() => {
        addon.dispose();
        webgl = null;
        renderer = "dom";
      });
      term.loadAddon(addon);
      webgl = addon;
      renderer = "webgl";
    } catch (err) {
      console.warn("[kiln] WebGL renderer unavailable, using DOM renderer", err);
    }
  }

  return {
    term,
    fit,
    search,
    renderer: () => renderer,
    dispose: () => {
      try {
        webgl?.dispose();
      } catch {
        /* ignore */
      }
      term.dispose();
    },
  };
}

/** fit() that ignores hidden/zero-size hosts (inactive tabs, display:none). */
export function safeFit(k: KilnTerminal, host: HTMLElement): boolean {
  if (host.clientWidth < 20 || host.clientHeight < 20) return false;
  try {
    const dims = k.fit.proposeDimensions();
    if (!dims || !Number.isFinite(dims.cols) || !Number.isFinite(dims.rows)) return false;
    if (dims.cols !== k.term.cols || dims.rows !== k.term.rows) k.fit.fit();
    return true;
  } catch {
    return false;
  }
}

export const SEARCH_DECORATIONS = {
  matchBackground: "#aca1d540",
  matchBorder: "#aca1d5",
  matchOverviewRuler: "#aca1d5",
  activeMatchBackground: "#d8cbb066",
  activeMatchBorder: "#d8cbb0",
  activeMatchColorOverviewRuler: "#d8cbb0",
};
