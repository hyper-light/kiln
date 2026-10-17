/**
 * How an agent runs (its permissions, model and effort) as the line the head already shows,
 * AUTO · OPUS 5.5 · MAX. Tap the line and it opens in place into one row of words per setting;
 * tap a word and it applies at once: the underline moves to it, and "· UNDO" waits at the end of
 * the line for a few seconds (the line always has room for it; a phone-width row often doesn't).
 * Nothing pops up or covers the conversation. It moves down while the rows are open, and they fold
 * away shortly after a change.
 *
 * `SendWith` is the same kind of row for a single message: send this one with Max effort, or with
 * another model.
 */
import * as React from "react";
import type { ControlOption, ModelOption, ModeOption } from "@/api/harness";
import { cn } from "@/lib/utils";
import "./run-settings.css";

type Kind = "mode" | "model" | "effort";

/** One choice in a row of words. */
interface Word {
  id: string;
  /** What the row shows, kept short ("Edits"). */
  text: string;
  /** Its full name ("Accept edits"), for assistive tech and the tooltip. */
  full: string;
  /** What it does: shown for the current permission mode, and in the tooltip. */
  hint?: string;
  /** Turns checks off, so it takes a second tap. */
  dangerous?: boolean;
}

const KEY: Record<Kind, string> = { mode: "Permissions", model: "Model", effort: "Effort" };
const KINDS: Kind[] = ["mode", "model", "effort"];
const CONFIRM = "No checks at all. Tap again to confirm.";
/** How long Undo stays offered after a change. */
const UNDO_MS = 5000;
/** The rows fold away this long after the last change (not while keyboard focus or a mouse is in them, or a second tap is awaited). */
const FOLD_MS = 2500;
/** A second tap this soon after the first is a double tap, not a confirmation. */
const CONFIRM_GAP_MS = 350;
/** The underline sits this far below the checked word's letters. */
const UNDERLINE_GAP = 3;

const same = (v: string) => v;
const capital = (v: string) => v.charAt(0).toUpperCase() + v.slice(1);

/** Whether focus arrived from the keyboard: a tap that happens to focus a word doesn't count as "focus inside". */
function focusVisible(el: EventTarget | null): boolean {
  if (!(el instanceof Element)) return false;
  try {
    return el.matches(":focus-visible");
  } catch {
    return true;
  }
}

/** Where a permission mode sits by how much the agent may do on its own; undefined for a mode we don't recognise. */
function modeRank(m: { id: string; label: string }): number | undefined {
  for (const raw of [m.id, m.label]) {
    const s = raw.toLowerCase().replace(/[^a-z]/g, "");
    if (!s) continue;
    if (/dontask|donotask|neverask/.test(s)) return 4;
    if (/^plan|readonly/.test(s)) return 0;
    if (/acceptedit|autoedit|editsok|allowedit/.test(s)) return 2;
    if (/^auto/.test(s)) return 3;
    if (/^(ask|default|untrusted|onrequest|manual|normal)/.test(s)) return 1;
  }
  return undefined;
}

/**
 * Read-only first, then ask, accept edits, auto, don't ask. Modes we don't recognise keep their
 * order after those; any that turn checks off come last.
 */
function byLatitude<T extends { id: string; label: string; dangerous?: boolean }>(modes: T[]): T[] {
  return modes
    .map((m, i) => ({ m, i, r: m.dangerous ? 6 : (modeRank(m) ?? 5) }))
    .sort((a, b) => a.r - b.r || a.i - b.i)
    .map((x) => x.m);
}

function modeWords(modes: ModeOption[], current: string): Word[] {
  const list: ModeOption[] = current && !modes.some((m) => m.id === current) ? [...modes, { id: current, label: current, description: "" }] : modes;
  return byLatitude(list).map((m) => {
    const full = m.label || m.id;
    return { id: m.id, text: modeRank(m) === 2 ? "Edits" : full, full, hint: m.description?.trim() || undefined, dangerous: m.dangerous };
  });
}

/** A model reads as `modelLabel` makes it ("Opus 5.5") when that knows the id, else as the harness names it. */
function modelWord(m: { id: string; label?: string; description?: string }, pretty: (id: string) => string): Word {
  const p = pretty(m.id);
  return { id: m.id, text: p !== m.id ? p : m.label || m.id, full: m.label || p, hint: m.description?.trim() || undefined };
}

function modelWords(models: ModelOption[], current: string, pretty: (id: string) => string): Word[] {
  const list = models.map((m) => modelWord(m, pretty));
  return current && !list.some((w) => w.id === current) ? [modelWord({ id: current }, pretty), ...list] : list;
}

function effortWords(efforts: ControlOption[], current: string): Word[] {
  const list = efforts.map((e) => {
    const name = e.label || capital(e.id);
    return { id: e.id, text: name, full: name, hint: e.description?.trim() || undefined, dangerous: e.dangerous };
  });
  return current && !list.some((w) => w.id === current) ? [{ id: current, text: capital(current), full: capital(current) }, ...list] : list;
}

/** Keeps a row's underline under its checked word. Once placed it glides from word to word (CSS: transform only). */
function useGlide(group: React.RefObject<HTMLElement | null>, bar: React.RefObject<HTMLElement | null>, deps: string) {
  React.useLayoutEffect(() => {
    const g = group.current;
    const b = bar.current;
    if (!g || !b) return;
    let live = true;
    const place = () => {
      if (!live) return;
      const on = g.querySelector('[aria-checked="true"] > .rs-word-text');
      if (!on) {
        b.style.opacity = "0";
        return;
      }
      const a = g.getBoundingClientRect();
      const r = on.getBoundingClientRect();
      b.style.opacity = "";
      b.style.transform = `translate(${r.left - a.left}px, ${r.bottom - a.top + UNDERLINE_GAP}px) scaleX(${r.width / 100})`;
    };
    place();
    // The first placement doesn't glide in from the corner.
    const frame = b.hasAttribute("data-glide") ? 0 : requestAnimationFrame(() => b.setAttribute("data-glide", ""));
    const watch = new ResizeObserver(place);
    watch.observe(g);
    void document.fonts?.ready.then(place);
    return () => {
      live = false;
      cancelAnimationFrame(frame);
      watch.disconnect();
    };
  }, [group, bar, deps]);
}

/** A setting's name and its choices as words: a radiogroup (arrow keys move, Enter or Space picks). */
function WordRow({
  kind,
  label,
  words,
  value,
  armed,
  onPick,
  desc,
  warn,
}: {
  kind: string;
  label: string;
  words: Word[];
  value: string;
  armed?: string;
  onPick: (w: Word, at: number) => void;
  /** One line under the words. */
  desc?: string;
  warn?: boolean;
}) {
  const keyId = React.useId();
  const group = React.useRef<HTMLDivElement>(null);
  const bar = React.useRef<HTMLElement>(null);
  const [focus, setFocus] = React.useState<string | null>(null);
  // One stop in the tab order: the word with focus, else the current one, else the first.
  const stop = [focus, value, words[0]?.id].find((id) => id != null && words.some((w) => w.id === id));
  useGlide(group, bar, `${value}\n${words.map((w) => `${w.id}\t${w.text}`).join("\n")}`);

  const rove = (e: React.KeyboardEvent<HTMLDivElement>) => {
    const radios = Array.from(e.currentTarget.querySelectorAll<HTMLElement>('[role="radio"]'));
    const i = radios.findIndex((r) => r === document.activeElement);
    const n = radios.length;
    if (i < 0 || n === 0) return;
    let j: number;
    switch (e.key) {
      case "ArrowRight":
      case "ArrowDown":
        j = (i + 1) % n;
        break;
      case "ArrowLeft":
      case "ArrowUp":
        j = (i - 1 + n) % n;
        break;
      case "Home":
        j = 0;
        break;
      case "End":
        j = n - 1;
        break;
      default:
        return;
    }
    e.preventDefault();
    radios[j]?.focus();
  };

  return (
    <div className="rs-set" data-kind={kind}>
      <span id={keyId} className="rs-key">
        {label}
      </span>
      <div className="rs-body">
        <div
          ref={group}
          role="radiogroup"
          aria-labelledby={keyId}
          className="rs-words"
          onKeyDown={rove}
          onBlur={(e) => {
            if (!e.currentTarget.contains(e.relatedTarget as Node | null)) setFocus(null);
          }}
        >
          {words.map((w, i) => (
            <span key={w.id} className="rs-cell">
              <button
                type="button"
                role="radio"
                aria-checked={w.id === value}
                // The full name when it contains what's shown ("Accept edits" for EDITS), so speech still matches the word.
                aria-label={w.full !== w.text && w.full.toLowerCase().includes(w.text.toLowerCase()) ? w.full : undefined}
                title={w.hint ? `${w.full}: ${w.hint}` : w.full}
                tabIndex={w.id === stop ? 0 : -1}
                className="rs-word"
                data-armed={w.id === armed ? "" : undefined}
                onFocus={() => setFocus(w.id)}
                onClick={(e) => onPick(w, e.timeStamp)}
              >
                <span className="rs-word-text">{w.text}</span>
              </button>
              {i < words.length - 1 && (
                <span className="rs-dot" aria-hidden="true">
                  ·
                </span>
              )}
            </span>
          ))}
          <i ref={bar} className="rs-glide" aria-hidden="true" />
        </div>
        {desc && (
          <p className="rs-desc" data-tone={warn ? "warn" : undefined}>
            {desc}
          </p>
        )}
      </div>
    </div>
  );
}

export interface RunSettingsProps {
  modes: ModeOption[];
  mode: string;
  /** Leave out when the harness can't change permissions: the value then shows as plain text. */
  onMode?: (id: string) => void;
  models: ModelOption[];
  model: string;
  onModel?: (id: string) => void;
  /** How a model id reads ("claude-opus-5-5" → "Opus 5.5"). */
  modelLabel?: (id: string) => string;
  efforts: ControlOption[];
  effort: string;
  onEffort?: (id: string) => void;
  /** Which settings the harness takes up at the next turn rather than at once (said only while it works). */
  appliesNextTurn?: { mode?: boolean; model?: boolean; effort?: boolean };
  running?: boolean;
  className?: string;
}

/** A change just made: what the line shows until the agent reports otherwise, and what Undo restores. */
interface Change {
  kind: Kind;
  /** The agent's own value when the change was made. The line shows `to` while the agent still reports this. */
  base: string;
  /** What Undo puts back. */
  prev: string;
  to: string;
  /** Offers Undo (an undo itself doesn't). */
  undoable: boolean;
}

/**
 * The head's line of how an agent runs, which opens in place into rows of words to change it.
 * Dangerous permission modes take a second tap; a change can be undone for a few seconds.
 */
export function RunSettings({
  modes,
  mode,
  onMode,
  models,
  model,
  onModel,
  modelLabel = same,
  efforts,
  effort,
  onEffort,
  appliesNextTurn,
  running = false,
  className,
}: RunSettingsProps) {
  const [open, setOpen] = React.useState(false);
  const [change, setChange] = React.useState<Change | null>(null);
  const [waiting, setWaiting] = React.useState<{ kind: Kind; id: string; at: number } | null>(null);
  /** Counts changes made since the rows opened; the fold timer restarts on each. */
  const [tick, setTick] = React.useState(0);
  const [hover, setHover] = React.useState(false);
  const [keyboard, setKeyboard] = React.useState(false);
  const [said, setSaid] = React.useState("");
  const root = React.useRef<HTMLDivElement>(null);
  const line = React.useRef<HTMLButtonElement>(null);
  /** Where focus goes after the next render, when the element that had it goes away. */
  const refocus = React.useRef<Kind | "line" | null>(null);
  const panelId = React.useId();
  const held = hover || keyboard;

  const actual: Record<Kind, string> = { mode, model, effort };
  const shown = (k: Kind) => (change?.kind === k && actual[k] === change.base ? change.to : actual[k]);
  const sets: Record<Kind, { words: Word[]; count: number; pick?: (id: string) => void }> = {
    mode: { words: modeWords(modes, shown("mode")), count: modes.length, pick: onMode },
    model: { words: modelWords(models, shown("model"), modelLabel), count: models.length, pick: onModel },
    effort: { words: effortWords(efforts, shown("effort")), count: efforts.length, pick: onEffort },
  };
  const nameOf = (k: Kind, id: string) => sets[k].words.find((w) => w.id === id)?.full ?? id;
  // A word waiting for its second tap only waits while it's still offered.
  const armed = waiting && sets[waiting.kind].words.some((w) => w.id === waiting.id) ? waiting : null;
  const nextTurn = (k: Kind) => Boolean(running && appliesNextTurn?.[k]);
  const rows = KINDS.filter((k) => sets[k].pick && sets[k].count > 0);

  // Undo is offered for a few seconds, longer while a keyboard or mouse is on it.
  React.useEffect(() => {
    if (!change || held) return;
    const t = window.setTimeout(() => setChange(null), UNDO_MS);
    return () => window.clearTimeout(t);
  }, [change, held]);

  // The rows fold away a little after a change, unless keyboard focus or a mouse is in them, or a
  // dangerous word is waiting for its second tap.
  React.useEffect(() => {
    if (!open || tick === 0 || held || armed) return;
    const t = window.setTimeout(() => {
      if (root.current?.contains(document.activeElement)) refocus.current = "line";
      setOpen(false);
      setWaiting(null);
    }, FOLD_MS);
    return () => window.clearTimeout(t);
  }, [open, tick, held, armed]);

  // A dangerous word waiting for its second tap stops waiting when anything else is tapped or focused.
  React.useEffect(() => {
    if (!armed) return;
    const away = (e: Event) => {
      const el = root.current?.querySelector("[data-armed]");
      if (el && e.target instanceof Node && el.contains(e.target)) return;
      setWaiting(null);
    };
    document.addEventListener("pointerdown", away, true);
    document.addEventListener("focusin", away, true);
    return () => {
      document.removeEventListener("pointerdown", away, true);
      document.removeEventListener("focusin", away, true);
    };
  }, [armed]);

  React.useLayoutEffect(() => {
    const to = refocus.current;
    if (!to) return;
    refocus.current = null;
    const el = to === "line" ? line.current : root.current?.querySelector<HTMLElement>(`.rs-set[data-kind="${to}"] [aria-checked="true"]`);
    el?.focus({ preventScroll: true });
  });

  const apply = (k: Kind, to: string) => {
    const pick = sets[k].pick;
    const prev = shown(k);
    if (!pick || to === prev) return;
    pick(to);
    setChange({ kind: k, base: actual[k], prev, to, undoable: Boolean(prev) });
    setWaiting(null);
    setTick((t) => t + 1);
    setSaid(`${KEY[k]}: ${nameOf(k, to)}${nextTurn(k) ? ", from the next turn" : ""}`);
  };

  const choose = (k: Kind, w: Word, at: number) => {
    if (w.id === shown(k)) {
      setWaiting(null);
      return;
    }
    if (w.dangerous) {
      if (armed?.kind !== k || armed.id !== w.id) {
        setWaiting({ kind: k, id: w.id, at });
        setSaid(`${w.full}: ${CONFIRM}`);
        return;
      }
      if (at - armed.at < CONFIRM_GAP_MS) return;
    }
    apply(k, w.id);
  };

  const undo = () => {
    if (!change?.undoable) return;
    const { kind: k, prev, to } = change;
    sets[k].pick?.(prev);
    setChange({ kind: k, base: actual[k], prev: to, to: prev, undoable: false });
    setWaiting(null);
    setTick((t) => t + 1);
    setSaid(`${KEY[k]}: back to ${nameOf(k, prev)}`);
    if (root.current?.contains(document.activeElement)) refocus.current = open ? k : "line";
  };

  const fold = () => {
    if (root.current?.contains(document.activeElement)) refocus.current = "line";
    setOpen(false);
    setWaiting(null);
  };

  const describe = (k: Kind) => {
    if (armed?.kind === k) return CONFIRM;
    const what = k === "mode" ? sets.mode.words.find((w) => w.id === shown("mode"))?.hint : undefined;
    if (!nextTurn(k)) return what;
    return what ? `${what} · from the next turn` : "From the next turn";
  };

  let parts = KINDS.map((k) => ({ k, text: shown(k) ? nameOf(k, shown(k)) : "", empty: false })).filter((p) => p.text);
  if (parts.length === 0) parts = rows.map((k) => ({ k, text: KEY[k], empty: true }));
  if (parts.length === 0) return null;

  const words = parts.map((p, i) => (
    <React.Fragment key={p.k}>
      {i > 0 && <span className="rs-sep"> · </span>}
      <span className="rs-v" data-empty={p.empty ? "" : undefined}>
        {p.text}
      </span>
    </React.Fragment>
  ));
  const undoable = change?.undoable ? change : null;
  const undoButton = undoable && (
    <span className="rs-after">
      <span className="rs-dot" aria-hidden="true">
        ·
      </span>
      <button type="button" className="rs-undo" aria-label={`Undo: back to ${nameOf(undoable.kind, undoable.prev)}`} onClick={undo}>
        Undo
      </button>
    </span>
  );

  return (
    <div
      ref={root}
      className={cn("rs-tuner", className)}
      data-open={open ? "" : undefined}
      onKeyDown={(e) => {
        if (e.key !== "Escape" || !open) return;
        e.preventDefault();
        e.stopPropagation();
        if (armed) setWaiting(null);
        else fold();
      }}
      onPointerEnter={(e) => {
        if (e.pointerType === "mouse") setHover(true);
      }}
      onPointerLeave={(e) => {
        if (e.pointerType === "mouse") setHover(false);
      }}
      onFocus={(e) => setKeyboard(focusVisible(e.target))}
      onBlur={(e) => {
        if (!e.currentTarget.contains(e.relatedTarget as Node | null)) setKeyboard(false);
      }}
    >
      <div className="rs-top">
        {rows.length > 0 ? (
          <button
            ref={line}
            type="button"
            className="rs-line"
            aria-expanded={open}
            aria-controls={open ? panelId : undefined}
            aria-label={`How it runs: ${parts.map((p) => p.text).join(", ")}`}
            onClick={() => {
              if (open) {
                fold();
              } else {
                setOpen(true);
                setTick(0);
              }
            }}
          >
            <span className="rs-line-text">{words}</span>
          </button>
        ) : (
          <p className="rs-line">
            <span className="rs-line-text">{words}</span>
          </p>
        )}
        {undoButton}
        {open && (
          // The line itself is the control that folds (and Esc); the minus is just somewhere else to tap.
          <span className="rs-fold" aria-hidden="true" onClick={fold}>
            −
          </span>
        )}
      </div>
      {open && (
        <div id={panelId} className="rs-panel" role="group" aria-label="How it runs">
          {rows.map((k) => (
            <WordRow
              key={k}
              kind={k}
              label={KEY[k]}
              words={sets[k].words}
              value={shown(k)}
              armed={armed?.kind === k ? armed.id : undefined}
              onPick={(w, at) => choose(k, w, at)}
              desc={describe(k)}
              warn={armed?.kind === k}
            />
          ))}
        </div>
      )}
      <span className="rs-sr" role="status">
        {said}
      </span>
    </div>
  );
}

/** What one message is sent with instead of the agent's own settings; {} sends it as the agent is set. */
export interface SendOverrides {
  model?: string;
  effort?: string;
}

export interface SendWithProps {
  models: ModelOption[];
  /** The agent's own model and effort (offered choices leave these out). */
  model: string;
  modelLabel?: (id: string) => string;
  efforts: ControlOption[];
  effort: string;
  /** The choice for the next message. */
  value: SendOverrides;
  onChange: (overrides: SendOverrides) => void;
  className?: string;
}

/**
 * A row for the composer: THIS MESSAGE  SAME · MAX EFFORT · <other models>. The composer keeps the
 * choice, sends it with the next message and clears it after. Renders nothing when there's nothing
 * to offer.
 */
export function SendWith({ models, model, modelLabel = same, efforts, effort, value, onChange, className }: SendWithProps) {
  const words: Word[] = [{ id: "same", text: "Same", full: "Same as the agent" }];
  const add = (w: Word) => {
    if (!words.some((x) => x.id === w.id)) words.push(w);
  };
  const effortWord = (id: string): Word => {
    const name = efforts.find((e) => e.id === id)?.label || capital(id);
    return { id: `effort:${id}`, text: `${name} effort`, full: `${name} effort` };
  };
  const top = efforts[efforts.length - 1];
  if (top && top.id !== effort) add(effortWord(top.id));
  if (value.effort) add(effortWord(value.effort));
  for (const m of models) if (m.id !== model) add({ ...modelWord(m, modelLabel), id: `model:${m.id}` });
  if (value.model) add({ ...modelWord(models.find((m) => m.id === value.model) ?? { id: value.model }, modelLabel), id: `model:${value.model}` });
  if (words.length < 2) return null;

  const current = value.effort ? `effort:${value.effort}` : value.model ? `model:${value.model}` : "same";
  return (
    <div className={cn("rs-tuner rs-once", className)}>
      <WordRow
        kind="once"
        label="This message"
        words={words}
        value={current}
        onPick={(w) => onChange(w.id.startsWith("effort:") ? { effort: w.id.slice(7) } : w.id.startsWith("model:") ? { model: w.id.slice(6) } : {})}
      />
    </div>
  );
}
