import * as React from "react";
import { ChevronDown, ChevronUp, Keyboard as KeyboardIcon, Smartphone } from "lucide-react";
import { useScrollEdges } from "@/lib/hooks";
import { usePref } from "@/lib/prefs";
import { cn } from "@/lib/utils";
import { charKey, namedKey, strokeBytes, type Mods, type NamedKey } from "./keys";
import { PRESETS, presetFor } from "./presets";

type Latch = "off" | "once" | "lock";
type Layer = "abc" | "sym" | "sym2";

export type KeyboardMode = "keys" | "full";
/** open: every row; shrunk: just the shortcuts (while reading or scrolling); closed: just the bar. */
export type KeyboardFold = "open" | "shrunk" | "closed";

interface Props {
  /** Send bytes to the session (as if typed). */
  send: (bytes: string) => void;
  /** The terminal's application-cursor mode, read at key time. */
  appCursor: () => boolean;
  /** The preset picked automatically from what the session runs. */
  autoPresetId: string;
  /** The user's preset for this session (null: automatic). */
  presetId: string | null;
  onPreset: (id: string | null) => void;
  mode: KeyboardMode;
  onMode: (m: KeyboardMode) => void;
  fold: KeyboardFold;
  onFold: (f: "open" | "closed") => void;
  /** Paste the clipboard into the session. */
  onPaste?: () => void;
  /** Copy the selection, or open the session as selectable text. */
  onCopy?: () => void;
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

/** Key heights the resize grip allows, in px. */
const KEY_MIN = 26;
const KEY_MAX = 64;
/** Grip travel before a press counts as a resize drag rather than a tap. */
const DRAG_SLOP_PX = 4;

/** Press-and-hold repeat for keys like Backspace and the arrows, tied to the finger holding it. */
function useRepeat() {
  const timers = React.useRef<{ t?: number; i?: number; pointer?: number }>({});
  /** End the repeat; with a pointer, only when that is the finger holding the key. */
  const stop = React.useCallback((pointer?: number) => {
    if (pointer !== undefined && timers.current.pointer !== undefined && pointer !== timers.current.pointer) return;
    window.clearTimeout(timers.current.t);
    window.clearInterval(timers.current.i);
    timers.current = {};
  }, []);
  const start = React.useCallback(
    (fire: () => void, pointer?: number) => {
      stop();
      fire();
      timers.current = {
        pointer,
        t: window.setTimeout(() => {
          timers.current.i = window.setInterval(fire, 55);
        }, 380),
      };
    },
    [stop],
  );
  return { start, stop };
}

const buzz = () => {
  try {
    navigator.vibrate?.(4);
  } catch {
    /* unsupported */
  }
};

function KeyboardImpl({ send, appCursor, autoPresetId, presetId, onPreset, mode, onMode, fold, onFold, onPaste, onCopy }: Props) {
  const [ctrl, setCtrl] = React.useState<Latch>("off");
  const [alt, setAlt] = React.useState<Latch>("off");
  const [shift, setShift] = React.useState<Latch>("off");
  const [layer, setLayer] = React.useState<Layer>("abc");
  const [presetOpen, setPresetOpen] = React.useState(false);
  const shortcutEdges = useScrollEdges<HTMLDivElement>();
  const extraEdges = useScrollEdges<HTMLDivElement>();
  // Per device: how tall the keys are (null: the default for the screen).
  const [keyHeight, setKeyHeight] = usePref<number | null>("kiln.term.keyboardKeyHeight", null);
  const open = fold === "open";
  /** Fold away when open; otherwise bring every row back. */
  const toggleFold = () => onFold(open ? "closed" : "open");
  const rootRef = React.useRef<HTMLDivElement | null>(null);
  const { start, stop } = useRepeat();
  // A key held as the keyboard goes stops repeating with it.
  const root = React.useCallback(
    (el: HTMLDivElement | null) => {
      rootRef.current = el;
      if (!el) return;
      return () => {
        stop();
        rootRef.current = null;
      };
    },
    [stop],
  );
  const preset = presetFor("", presetId ?? autoPresetId);
  const autoName = presetFor("", autoPresetId).name;

  const mods = (): Mods => ({
    ctrl: ctrl !== "off",
    alt: alt !== "off",
    shift: shift !== "off",
  });
  const consume = () => {
    if (ctrl === "once") setCtrl("off");
    if (alt === "once") setAlt("off");
    if (shift === "once") setShift("off");
  };
  const cycle = (set: React.Dispatch<React.SetStateAction<Latch>>) => set((v) => (v === "off" ? "once" : v === "once" ? "lock" : "off"));

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
      if (repeat) start(fn, e.pointerId);
      else fn();
    },
    onPointerUp: (e: React.PointerEvent) => stop(e.pointerId),
    onPointerLeave: (e: React.PointerEvent) => stop(e.pointerId),
    onPointerCancel: (e: React.PointerEvent) => stop(e.pointerId),
    onContextMenu: (e: React.MouseEvent) => e.preventDefault(),
  });

  /**
   * A press between keys or at the keyboard's edge goes to the nearest key, as on a phone keyboard:
   * fingers typing fast land in the gaps, and those presses used to be lost. The finger's own
   * pointerup reaches the keyboard (not the key), which ends a forwarded key's repeat.
   */
  const pressNearest = (e: React.PointerEvent<HTMLDivElement>) => {
    const target = e.target as Element;
    if (target.closest("button, .vk-bar, .vk-menu")) return;
    e.preventDefault(); // focus (and the device keyboard) stay where they are
    const root = rootRef.current;
    if (!root) return;
    let best: HTMLButtonElement | null = null;
    let bestD = Infinity;
    for (const key of root.querySelectorAll<HTMLButtonElement>(".vk-row > button")) {
      const r = key.getBoundingClientRect();
      if (r.width === 0) continue;
      const dx = Math.max(r.left - e.clientX, 0, e.clientX - r.right);
      const dy = Math.max(r.top - e.clientY, 0, e.clientY - r.bottom);
      const d = dx * dx + dy * dy;
      if (d < bestD) {
        bestD = d;
        best = key;
      }
    }
    best?.dispatchEvent(
      new PointerEvent("pointerdown", {
        bubbles: true,
        cancelable: true,
        pointerId: e.pointerId,
        pointerType: e.pointerType,
        isPrimary: e.isPrimary,
        clientX: e.clientX,
        clientY: e.clientY,
        button: 0,
        buttons: 1,
      }),
    );
  };

  /**
   * The grip: drag up for bigger keys, down for smaller ones; a tap folds the keyboard away or
   * brings it back, and so does pulling it up while folded. The height is applied straight to the
   * element while dragging (no re-render per move) and saved when the finger lifts.
   */
  const grip = {
    onPointerDown: (e: React.PointerEvent<HTMLButtonElement>) => {
      e.preventDefault();
      const root = rootRef.current;
      if (!root) return;
      const el = e.currentTarget;
      el.setPointerCapture(e.pointerId);
      const y0 = e.clientY;
      const key = root.querySelector<HTMLElement>(".vk-key");
      const h0 = key?.offsetHeight ?? 40;
      const rows = Math.max(1, root.querySelectorAll(".vk > .vk-row, .vk-full > .vk-row").length);
      let dragging = false;
      let h = h0;
      const move = (ev: PointerEvent) => {
        const dy = y0 - ev.clientY;
        if (!dragging && Math.abs(dy) < DRAG_SLOP_PX) return;
        dragging = true;
        if (!open) return;
        h = Math.round(Math.min(KEY_MAX, Math.max(KEY_MIN, h0 + dy / rows)));
        root.style.setProperty("--vk-key-h", `${h}px`);
      };
      const end = (ev: PointerEvent) => {
        el.removeEventListener("pointermove", move);
        el.removeEventListener("pointerup", end);
        el.removeEventListener("pointercancel", end);
        if (el.hasPointerCapture(ev.pointerId)) el.releasePointerCapture(ev.pointerId);
        if (!dragging) {
          if (ev.type === "pointerup") toggleFold();
        } else if (!open) {
          if (y0 - ev.clientY > 24) onFold("open");
        } else {
          setKeyHeight(h);
        }
      };
      el.addEventListener("pointermove", move);
      el.addEventListener("pointerup", end);
      el.addEventListener("pointercancel", end);
    },
    onKeyDown: (e: React.KeyboardEvent) => {
      // Keyboard access to the same control: arrows resize, Enter/Space folds.
      const cur = keyHeight ?? rootRef.current?.querySelector<HTMLElement>(".vk-key")?.offsetHeight ?? 40;
      if (e.key === "ArrowUp" || e.key === "ArrowDown") {
        e.preventDefault();
        setKeyHeight(Math.min(KEY_MAX, Math.max(KEY_MIN, cur + (e.key === "ArrowUp" ? 2 : -2))));
      }
    },
  };

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

  const style = keyHeight ? ({ "--vk-key-h": `${keyHeight}px` } as React.CSSProperties) : undefined;

  return (
    <div
      ref={root}
      className="vk"
      role="group"
      aria-label="Terminal keyboard"
      data-mode={mode}
      data-fold={fold}
      style={style}
      onPointerDown={pressNearest}
      onPointerUp={(e) => stop(e.pointerId)}
      onPointerCancel={(e) => stop(e.pointerId)}
      onContextMenu={(e) => e.preventDefault()}
    >
      {/* Always on screen: resize/fold grip, device-keyboard switch, fold button. */}
      <div className="vk-bar">
        <button
          type="button"
          className="vk-grip"
          aria-label={open ? "Resize keyboard: drag, or tap to fold it away" : "Show keyboard"}
          aria-expanded={open}
          {...grip}
          onClick={(e) => {
            // Pointer taps are handled on pointerup; this is for keyboard and assistive activation.
            if (e.detail === 0) toggleFold();
          }}
        >
          <span aria-hidden="true" />
        </button>
        <div className="vk-bar-end">
          <button
            type="button"
            className="vk-bar-btn"
            aria-label={mode === "full" ? "Use the device keyboard" : "Use the kiln keyboard"}
            title={mode === "full" ? "Device keyboard" : "kiln keyboard"}
            {...press(() => onMode(mode === "full" ? "keys" : "full"))}
          >
            {mode === "full" ? <Smartphone size={15} /> : <KeyboardIcon size={15} />}
          </button>
          <button type="button" className="vk-bar-btn" aria-label={open ? "Fold keyboard away" : "Show keyboard"} aria-expanded={open} {...press(toggleFold)}>
            {open ? <ChevronDown size={15} /> : <ChevronUp size={15} />}
          </button>
        </div>
      </div>

      {fold !== "closed" && (
        <>
          {presetOpen && (
            <div className="vk-menu" role="listbox" aria-label="Shortcut set">
              <button
                type="button"
                role="option"
                aria-selected={presetId === null}
                className="vk-menu-item"
                {...press(() => {
                  onPreset(null);
                  setPresetOpen(false);
                })}
              >
                Auto ({autoName})
              </button>
              {PRESETS.map((p) => (
                <button
                  key={p.id}
                  type="button"
                  role="option"
                  aria-selected={presetId === p.id}
                  className="vk-menu-item"
                  {...press(() => {
                    onPreset(p.id);
                    setPresetOpen(false);
                  })}
                >
                  {p.name}
                </button>
              ))}
            </div>
          )}
          {/* Shortcuts for the program in this session. */}
          <div className="vk-row vk-scroll" ref={shortcutEdges}>
            <button type="button" className="vk-key vk-chip vk-preset" aria-haspopup="listbox" aria-expanded={presetOpen} {...press(() => setPresetOpen((o) => !o))}>
              {preset.name} <ChevronDown size={12} />
            </button>
            {preset.shortcuts.map((s) => (
              <button
                key={s.label}
                type="button"
                className="vk-key vk-chip"
                title={s.hint}
                aria-label={`${s.label}: ${s.hint}`}
                {...press(() => send(s.strokes.map((st) => strokeBytes(st, appCursor())).join("")))}
              >
                {s.label}
              </button>
            ))}
          </div>

          {/* Keys a phone keyboard lacks. */}
          {open && (
            <div className="vk-row vk-scroll" ref={extraEdges}>
              {onPaste && (
                <button type="button" className="vk-key" aria-label="Paste" {...press(onPaste)}>
                  Paste
                </button>
              )}
              {onCopy && (
                <button type="button" className="vk-key" aria-label="Copy" {...press(onCopy)}>
                  Copy
                </button>
              )}
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
            </div>
          )}

          {open && mode === "full" && (
            <div className="vk-full">
              {ROWS[layer].map((row, i) => (
                <div key={i} className="vk-row vk-keys">
                  {i === 2 &&
                    (layer === "abc" ? (
                      latchKey("⇧", shift, setShift)
                    ) : (
                      <button type="button" className="vk-key vk-wide" {...press(() => setLayer(layer === "sym" ? "sym2" : "sym"))}>
                        {layer === "sym" ? "#+=" : "123"}
                      </button>
                    ))}
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
        </>
      )}
    </div>
  );
}

/** Re-renders only for its own state and props (the session's output and title never touch it). */
export const TerminalKeyboard = React.memo(KeyboardImpl);
