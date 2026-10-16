/**
 * Terminal key encoding for the on-screen keyboard: the exact bytes a real xterm sends, so shells,
 * readline/zle shortcuts, full-screen programs and AI coding harnesses all see what they expect.
 */

export interface Mods {
  ctrl?: boolean;
  alt?: boolean;
  shift?: boolean;
}

export type NamedKey =
  | "Escape"
  | "Tab"
  | "Enter"
  | "Backspace"
  | "Delete"
  | "Insert"
  | "ArrowUp"
  | "ArrowDown"
  | "ArrowLeft"
  | "ArrowRight"
  | "Home"
  | "End"
  | "PageUp"
  | "PageDown"
  | `F${number}`;

/** xterm modifier parameter: 1 + shift + 2·alt + 4·ctrl. */
function modParam(m: Mods): number {
  return 1 + (m.shift ? 1 : 0) + (m.alt ? 2 : 0) + (m.ctrl ? 4 : 0);
}

const CSI = "\x1b[";
const SS3 = "\x1bO";

/**
 * Bytes for a named key. `appCursor` is the terminal's application-cursor mode (DECCKM), which
 * full-screen programs (vim, less, many TUIs) switch on; arrows/Home/End change encoding with it.
 */
export function namedKey(key: NamedKey, mods: Mods = {}, appCursor = false): string {
  const p = modParam(mods);
  const cursor = (final: string) => (p > 1 ? `${CSI}1;${p}${final}` : `${appCursor ? SS3 : CSI}${final}`);
  const tilde = (n: number) => (p > 1 ? `${CSI}${n};${p}~` : `${CSI}${n}~`);
  switch (key) {
    case "Escape":
      return mods.alt ? "\x1b\x1b" : "\x1b";
    case "Tab":
      return mods.shift ? `${CSI}Z` : mods.alt ? "\x1b\t" : "\t";
    case "Enter":
      return mods.alt ? "\x1b\r" : "\r";
    case "Backspace":
      // Ctrl+Backspace deletes a word in most line editors (sent as ^H); Alt+Backspace likewise.
      return mods.ctrl ? "\x08" : mods.alt ? "\x1b\x7f" : "\x7f";
    case "Delete":
      return tilde(3);
    case "Insert":
      return tilde(2);
    case "PageUp":
      return tilde(5);
    case "PageDown":
      return tilde(6);
    case "ArrowUp":
      return cursor("A");
    case "ArrowDown":
      return cursor("B");
    case "ArrowRight":
      return cursor("C");
    case "ArrowLeft":
      return cursor("D");
    case "Home":
      return cursor("H");
    case "End":
      return cursor("F");
    default: {
      const n = Number(key.slice(1));
      const ss3 = ["P", "Q", "R", "S"];
      if (n >= 1 && n <= 4) return p > 1 ? `${CSI}1;${p}${ss3[n - 1]}` : `${SS3}${ss3[n - 1]}`;
      const codes: Record<number, number> = { 5: 15, 6: 17, 7: 18, 8: 19, 9: 20, 10: 21, 11: 23, 12: 24 };
      const c = codes[n];
      return c ? tilde(c) : "";
    }
  }
}

/** Bytes for a printable character with modifiers (Ctrl maps to C0 controls, Alt prefixes ESC). */
export function charKey(ch: string, mods: Mods = {}): string {
  let out = mods.shift && ch.length === 1 ? ch.toUpperCase() : ch;
  if (mods.ctrl && out.length === 1) {
    const c = out.toUpperCase();
    const code = c.charCodeAt(0);
    if (c === " " || c === "@" || c === "2") out = "\x00";
    else if (code >= 0x41 && code <= 0x5f) out = String.fromCharCode(code & 0x1f); // A–Z [ \ ] ^ _
    else if (c === "?" || c === "8") out = "\x7f";
    else if (c === "/" || c === "7") out = "\x1f";
    else if (c >= "3" && c <= "6") out = String.fromCharCode(0x1b + c.charCodeAt(0) - 0x33); // ^3..^6 = ESC FS GS RS
  }
  return mods.alt ? `\x1b${out}` : out;
}

/** A step in a shortcut: a named key or literal text, with modifiers. */
export type Stroke = { key: NamedKey; mods?: Mods } | { text: string; mods?: Mods };

export function strokeBytes(s: Stroke, appCursor: boolean): string {
  return "key" in s ? namedKey(s.key, s.mods, appCursor) : [...s.text].map((c) => charKey(c, s.mods)).join("");
}
