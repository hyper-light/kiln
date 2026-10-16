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
  /** Session command / window-title fragments that select this preset automatically. */
  match: RegExp;
  shortcuts: Shortcut[];
}

const ctrl = (c: string): Stroke => ({ text: c, mods: { ctrl: true } });
const esc: Stroke = { key: "Escape" };

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

/** Claude Code's interactive-mode bindings. */
export const CLAUDE: Preset = {
  id: "claude",
  name: "Claude Code",
  match: /claude/i,
  shortcuts: [
    { label: "Esc", hint: "Interrupt Claude", strokes: [esc] },
    { label: "Esc Esc", hint: "Rewind / edit a previous message", strokes: [esc, esc] },
    { label: "⇧Tab", hint: "Cycle permission mode (auto-accept / plan)", strokes: [{ key: "Tab", mods: { shift: true } }] },
    { label: "^C", hint: "Cancel input or generation (twice: exit)", strokes: [ctrl("c")] },
    { label: "⏎ newline", hint: "New line in the prompt (Ctrl+J)", strokes: [ctrl("j")] },
    { label: "/", hint: "Slash command", strokes: [{ text: "/" }] },
    { label: "@", hint: "Mention a file", strokes: [{ text: "@" }] },
    { label: "!", hint: "Bash mode", strokes: [{ text: "!" }] },
    { label: "#", hint: "Add to memory", strokes: [{ text: "#" }] },
    { label: "^O", hint: "Verbose output / transcript", strokes: [ctrl("o")] },
    { label: "^T", hint: "Task list", strokes: [ctrl("t")] },
    { label: "^B", hint: "Run the current command in the background", strokes: [ctrl("b")] },
    { label: "^R", hint: "Search history", strokes: [ctrl("r")] },
    { label: "^G", hint: "Edit the prompt in $EDITOR", strokes: [ctrl("g")] },
    { label: "^L", hint: "Clear the screen", strokes: [ctrl("l")] },
    { label: "^D", hint: "Exit", strokes: [ctrl("d")] },
  ],
};

/** OpenAI Codex CLI bindings. */
export const CODEX: Preset = {
  id: "codex",
  name: "Codex",
  match: /codex/i,
  shortcuts: [
    { label: "Esc", hint: "Interrupt", strokes: [esc] },
    { label: "Esc Esc", hint: "Edit a previous message", strokes: [esc, esc] },
    { label: "^C", hint: "Cancel (twice: quit)", strokes: [ctrl("c")] },
    { label: "⏎ newline", hint: "New line in the prompt (Ctrl+J)", strokes: [ctrl("j")] },
    { label: "/", hint: "Slash command", strokes: [{ text: "/" }] },
    { label: "@", hint: "Search for a file", strokes: [{ text: "@" }] },
    { label: "^T", hint: "Transcript", strokes: [ctrl("t")] },
    { label: "^D", hint: "Exit", strokes: [ctrl("d")] },
  ],
};

/** Google Gemini CLI bindings. */
export const GEMINI: Preset = {
  id: "gemini",
  name: "Gemini CLI",
  match: /gemini/i,
  shortcuts: [
    { label: "Esc", hint: "Cancel the request", strokes: [esc] },
    { label: "⇧Tab", hint: "Toggle auto-accept edits", strokes: [{ key: "Tab", mods: { shift: true } }] },
    { label: "^X", hint: "Edit the prompt in an external editor", strokes: [ctrl("x")] },
    { label: "^Y", hint: "Toggle YOLO mode", strokes: [ctrl("y")] },
    { label: "^T", hint: "Tool descriptions", strokes: [ctrl("t")] },
    { label: "/", hint: "Slash command", strokes: [{ text: "/" }] },
    { label: "@", hint: "Add a file", strokes: [{ text: "@" }] },
    { label: "^C", hint: "Quit", strokes: [ctrl("c")] },
  ],
};

export const PRESETS: Preset[] = [SHELL, CLAUDE, CODEX, GEMINI];

/**
 * The preset for a session: the user's choice for it, else the first AI harness whose name appears
 * in what the session runs (its command, or the window title its program set), else the shell.
 */
export function presetFor(hints: string, chosen?: string | null): Preset {
  if (chosen) return PRESETS.find((p) => p.id === chosen) ?? SHELL;
  return PRESETS.find((p) => p !== SHELL && p.match.test(hints)) ?? SHELL;
}
