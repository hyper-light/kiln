import * as React from "react";
import { ChevronDown, Keyboard as KeyboardIcon, Smartphone } from "lucide-react";
import { cn } from "@/lib/utils";
import { charKey, namedKey, strokeBytes, type Mods, type NamedKey } from "./keys";
import { PRESETS, presetFor } from "./presets";

type Latch = "off" | "once" | "lock";
type Layer = "abc" | "sym" | "sym2";

export type KeyboardMode = "keys" | "full";

interface Props {
  /** Send bytes to the session (as if typed). */
  send: (bytes: string) => void;
  /** The terminal's application-cursor mode, read at key time. */
  appCursor: () => boolean;
  /** The session's window title, used to pick the shortcut preset. */
  title: string;
  mode: KeyboardMode;
  onMode: (m: KeyboardMode) => void;
  onHide: () => void;
}

const ROWS: Record<Layer, string[][]> = {
  abc: [
    ["q", "w", "e", "r", "t", "y", "u", "i", "o", "p"],
    ["a", "s", "d", "f", "g", "h", "j", "k", "l"],
    ["z", "x", "c", "v", "b", "n", "m"],
  ],
  sym: [
    ["1", "2", "3", "4", "5", "6", "7", "8", "9", "0"],
    ["-", "/", ":", ";", "(", ")", "$", "&", "@", '"'],
    [".", ",", "?", "!", "'", "`"],
  ],
  sym2: [
    ["[", "]", "{", "}", "#", "%", "^", "*", "+", "="],
    ["_", "\\", "|", "~", "<", ">", "€", "£", "¥", "•"],
    [".", ",", "?", "!", "'", "`"],
  ],
};

/** Press-and-hold repeat for keys like Backspace and the arrows. */
function useRepeat() {
  const timers = React.useRef<{ t?: number; i?: number }>({});
  const stop = React.useCallback(() => {
    window.clearTimeout(timers.current.t);
    window.clearInterval(timers.current.i);
    timers.current = {};
  }, []);
  const start = React.useCallback(
    (fire: () => void) => {
      stop();
      fire();
      timers.current.t = window.setTimeout(() => {
        timers.current.i = window.setInterval(fire, 55);
      }, 380);
    },
    [stop],
  );
  React.useEffect(() => stop, [stop]);
  return { start, stop };
}

const buzz = () => {
  try {
    navigator.vibrate?.(4);
  } catch {
    /* unsupported */
  }
};

export function TerminalKeyboard({ send, appCursor, title, mode, onMode, onHide }: Props) {
  const [ctrl, setCtrl] = React.useState<Latch>("off");
  const [alt, setAlt] = React.useState<Latch>("off");
  const [shift, setShift] = React.useState<Latch>("off");
  const [layer, setLayer] = React.useState<Layer>("abc");
  const [presetId, setPresetId] = React.useState<string | null>(null);
  const [presetOpen, setPresetOpen] = React.useState(false);
  const { start, stop } = useRepeat();
  const preset = presetFor(title, presetId);

  const mods = (): Mods => ({ ctrl: ctrl !== "off", alt: alt !== "off", shift: shift !== "off" });
  const consume = () => {
    if (ctrl === "once") setCtrl("off");
    if (alt === "once") setAlt("off");
    if (shift === "once") setShift("off");
  };
  const cycle = (set: React.Dispatch<React.SetStateAction<Latch>>) =>
    set((v) => (v === "off" ? "once" : v === "once" ? "lock" : "off"));

  const fireNamed = (k: NamedKey) => {
    send(namedKey(k, mods(), appCursor()));
    consume();
  };
  const fireChar = (c: string) => {
    send(charKey(c, mods()));
    consume();
  };

  /** Pointer-down handlers that never take focus away from the terminal. */
  const press = (fn: () => void, repeat = false) => ({
    onPointerDown: (e: React.PointerEvent) => {
      e.preventDefault();
      buzz();
      if (repeat) start(fn);
      else fn();
    },
    onPointerUp: stop,
    onPointerLeave: stop,
    onPointerCancel: stop,
    onContextMenu: (e: React.MouseEvent) => e.preventDefault(),
  });

  const latchKey = (label: string, v: Latch, set: React.Dispatch<React.SetStateAction<Latch>>) => (
    <button type="button" className="vk-key vk-mod" data-latch={v} aria-pressed={v !== "off"} {...press(() => cycle(set))}>
      {label}
    </button>
  );

  const navKey = (label: string, k: NamedKey, repeat = false, aria?: string) => (
    <button type="button" className="vk-key" aria-label={aria ?? label} {...press(() => fireNamed(k), repeat)}>
      {label}
    </button>
  );

  return (
    <div className="vk" role="group" aria-label="Terminal keyboard" data-mode={mode}>
      {/* Shortcuts for the program in this session. */}
      <div className="vk-row vk-scroll">
        <div className="vk-preset">
          <button type="button" className="vk-key vk-chip" aria-haspopup="listbox" aria-expanded={presetOpen} {...press(() => setPresetOpen((o) => !o))}>
            {preset.name} <ChevronDown size={12} />
          </button>
          {presetOpen && (
            <div className="vk-menu" role="listbox">
              <button type="button" role="option" aria-selected={presetId === null} className="vk-menu-item" {...press(() => { setPresetId(null); setPresetOpen(false); })}>
                Auto ({presetFor(title).name})
              </button>
              {PRESETS.map((p) => (
                <button key={p.id} type="button" role="option" aria-selected={presetId === p.id} className="vk-menu-item" {...press(() => { setPresetId(p.id); setPresetOpen(false); })}>
                  {p.name}
                </button>
              ))}
            </div>
          )}
        </div>
        {preset.shortcuts.map((s) => (
          <button key={s.label} type="button" className="vk-key vk-chip" title={s.hint} aria-label={`${s.label}: ${s.hint}`} {...press(() => send(s.strokes.map((st) => strokeBytes(st, appCursor())).join("")))}>
            {s.label}
          </button>
        ))}
      </div>

      {/* Keys a phone keyboard lacks. */}
      <div className="vk-row vk-scroll">
        {navKey("Esc", "Escape")}
        {navKey("Tab", "Tab")}
        {latchKey("Ctrl", ctrl, setCtrl)}
        {latchKey("Alt", alt, setAlt)}
        {latchKey("⇧", shift, setShift)}
        {navKey("←", "ArrowLeft", true, "Left")}
        {navKey("↓", "ArrowDown", true, "Down")}
        {navKey("↑", "ArrowUp", true, "Up")}
        {navKey("→", "ArrowRight", true, "Right")}
        {navKey("Home", "Home")}
        {navKey("End", "End")}
        {navKey("PgUp", "PageUp", true)}
        {navKey("PgDn", "PageDown", true)}
        {["|", "~", "/", "-", "_"].map((c) => (
          <button key={c} type="button" className="vk-key" {...press(() => fireChar(c))}>
            {c}
          </button>
        ))}
        <span className="vk-spacer" />
        <button type="button" className="vk-key vk-icon" aria-label={mode === "full" ? "Use the device keyboard" : "Use the kiln keyboard"} title={mode === "full" ? "Device keyboard" : "kiln keyboard"} {...press(() => onMode(mode === "full" ? "keys" : "full"))}>
          {mode === "full" ? <Smartphone size={15} /> : <KeyboardIcon size={15} />}
        </button>
        <button type="button" className="vk-key vk-icon" aria-label="Hide keyboard" {...press(onHide)}>
          <ChevronDown size={15} />
        </button>
      </div>

      {mode === "full" && (
        <div className="vk-full">
          {ROWS[layer].map((row, i) => (
            <div key={i} className="vk-row vk-keys">
              {i === 2 && (
                layer === "abc" ? (
                  latchKey("⇧", shift, setShift)
                ) : (
                  <button type="button" className="vk-key vk-wide" {...press(() => setLayer(layer === "sym" ? "sym2" : "sym"))}>
                    {layer === "sym" ? "#+=" : "123"}
                  </button>
                )
              )}
              {row.map((c) => (
                <button key={c} type="button" className="vk-key vk-char" {...press(() => fireChar(c))}>
                  {shift !== "off" && layer === "abc" ? c.toUpperCase() : c}
                </button>
              ))}
              {i === 2 && (
                <button type="button" className="vk-key vk-wide" aria-label="Backspace" {...press(() => fireNamed("Backspace"), true)}>
                  ⌫
                </button>
              )}
            </div>
          ))}
          <div className="vk-row vk-keys">
            <button type="button" className="vk-key vk-wide" {...press(() => setLayer(layer === "abc" ? "sym" : "abc"))}>
              {layer === "abc" ? "123" : "ABC"}
            </button>
            {latchKey("Ctrl", ctrl, setCtrl)}
            <button type="button" className={cn("vk-key vk-space")} aria-label="Space" {...press(() => fireChar(" "), true)}>
              space
            </button>
            <button type="button" className="vk-key vk-wide vk-enter" aria-label="Enter" {...press(() => fireNamed("Enter"))}>
              ⏎
            </button>
          </div>
        </div>
      )}
    </div>
  );
}
