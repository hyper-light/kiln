import type { Stroke } from "./keys";

/** One-tap shortcut on the keyboard's shortcut row. */
export interface Shortcut {
  label: string;
  /** What it does, for the long-press/title hint. */
  hint: string;
  strokes: Stroke[];
}

export interface Preset {
  id: string;
  name: string;
  /** Process/title fragments that select this preset automatically. */
  match: RegExp;
  shortcuts: Shortcut[];
}

const ctrl = (c: string): Stroke => ({ text: c, mods: { ctrl: true } });

/** zsh / bash line editing (emacs keymap, the default in both). */
export const SHELL: Preset = {
  id: "shell",
  name: "Shell",
  match: /^$/,
  shortcuts: [
    { label: "Tab", hint: "Complete", strokes: [{ key: "Tab" }] },
    { label: "^C", hint: "Interrupt", strokes: [ctrl("c")] },
    { label: "^R", hint: "Search history", strokes: [ctrl("r")] },
    { label: "^D", hint: "End of input / exit", strokes: [ctrl("d")] },
    { label: "^L", hint: "Clear screen", strokes: [ctrl("l")] },
    { label: "^A", hint: "Start of line", strokes: [ctrl("a")] },
    { label: "^E", hint: "End of line", strokes: [ctrl("e")] },
    { label: "^W", hint: "Delete word back", strokes: [ctrl("w")] },
    { label: "^U", hint: "Delete to line start", strokes: [ctrl("u")] },
    { label: "^K", hint: "Delete to line end", strokes: [ctrl("k")] },
    { label: "Alt .", hint: "Insert last argument", strokes: [{ text: ".", mods: { alt: true } }] },
    { label: "Alt b", hint: "Word back", strokes: [{ text: "b", mods: { alt: true } }] },
    { label: "Alt f", hint: "Word forward", strokes: [{ text: "f", mods: { alt: true } }] },
    { label: "^Z", hint: "Suspend job", strokes: [ctrl("z")] },
  ],
};

/** Presets for AI coding harnesses are filled from their documented bindings. */
export const PRESETS: Preset[] = [SHELL];

/** The preset for a session, from what its program reports (window title) or the user's choice. */
export function presetFor(title: string, chosen?: string | null): Preset {
  if (chosen) return PRESETS.find((p) => p.id === chosen) ?? SHELL;
  return PRESETS.find((p) => p !== SHELL && p.match.test(title)) ?? SHELL;
}
