/**
 * How an agent runs (its permissions, model and effort) as the line the head already shows,
 * ASK · OPUS 5.5 · MAX. Each value is its own control: tap one and just that setting opens in
 * place, with the control that suits it.
 *
 * - Permissions and effort are scales (from "plans only" to "asks nothing", from low to max), so
 *   each is one track: tap a stop or slide to it. A mode that turns checks off waits for an
 *   explicit "Turn them off".
 * - The model is a family (Opus, Fable, Sonnet, Haiku); its other versions are one tap further.
 *
 * A choice applies at once, "· UNDO" waits on the line for a few seconds, and the setting folds
 * away shortly after. Nothing pops up or covers the conversation.
 *
 * `SendWith` is the composer's row for a single message: the same, max effort, or another model.
 */
import * as React from "react";
import type { ControlOption, ModelOption, ModeOption } from "@/api/harness";
import { cn } from "@/lib/utils";
import "./run-settings.css";

type Kind = "mode" | "model" | "effort";

/** One choice. */
interface Word {
  id: string;
  /** What it shows, kept short ("Edits", "Opus 5.5"). */
  text: string;
  /** Its full name ("Accept edits"), for assistive tech and the tooltip. */
  full: string;
  /** What it does: shown under the scale while it's the one chosen (or slid to). */
  hint?: string;
  /** Turns checks off, so it takes an explicit confirmation. */
  dangerous?: boolean;
}

const KEY: Record<Kind, string> = { mode: "Permissions", model: "Model", effort: "Effort" };
const KINDS: Kind[] = ["mode", "model", "effort"];
const CONFIRM = "No checks at all.";
/** How long Undo stays offered after a change. */
const UNDO_MS = 5000;
/** The setting folds away this long after the last change (not while keyboard focus or a mouse is in it, or a confirmation is waiting). */
const FOLD_MS = 2500;
/** The underline sits this far below the checked word's letters. */
const UNDERLINE_GAP = 3;

const same = (v: string) => v;
const capital = (v: string) => v.charAt(0).toUpperCase() + v.slice(1);

/** Whether focus arrived from the keyboard: a tap that happens to focus a control doesn't count as "focus inside". */
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
  // The agent's own model, when the harness doesn't list it, goes last: the families keep the harness's order.
  return current && !list.some((w) => w.id === current) ? [...list, modelWord({ id: current }, pretty)] : list;
}

function effortWords(efforts: ControlOption[], current: string): Word[] {
  const list = efforts.map((e) => {
    const name = e.label || capital(e.id);
    return { id: e.id, text: name, full: name, hint: e.description?.trim() || undefined, dangerous: e.dangerous };
  });
  return current && !list.some((w) => w.id === current) ? [{ id: current, text: capital(current), full: capital(current) }, ...list] : list;
}

/* ------------------------------------------------------------------ model families */

/** Models of one family ("Opus"): what choosing the family picks (its alias, else its newest), and every version. */
interface Family {
  name: string;
  pick: Word;
  versions: Word[];
}

const familyName = (w: Word) => w.text.split(/\s+/)[0] || w.text;

/** The version part of a model's name within its family: "5.5", "5.5 (1M)", or "Latest" for the family's alias. */
function versionText(w: Word, family: string): string {
  const rest = w.text.slice(family.length).trim();
  return rest || "Latest";
}

/** "5.5 (1M)" → [5, 5]. */
function versionNumbers(text: string): number[] {
  return (text.match(/\d+(?:\.\d+)*/)?.[0] ?? "")
    .split(".")
    .filter(Boolean)
    .map(Number);
}

/** Newest first; the plain version before its variants ("5.5" before "5.5 (1M)"). */
function newer(a: Word, b: Word, family: string): number {
  const va = versionNumbers(versionText(a, family));
  const vb = versionNumbers(versionText(b, family));
  for (let i = 0; i < Math.max(va.length, vb.length); i++) {
    const d = (vb[i] ?? 0) - (va[i] ?? 0);
    if (d !== 0) return d;
  }
  return a.text.length - b.text.length;
}

/** The models grouped into families, in the order the harness lists them. */
function families(words: Word[]): Family[] {
  const groups = new Map<string, Word[]>();
  for (const w of words) {
    const f = familyName(w);
    const g = groups.get(f);
    if (g) g.push(w);
    else groups.set(f, [w]);
  }
  const out: Family[] = [];
  for (const [name, members] of groups) {
    const alias = members.find((w) => w.text === name);
    const pinned = members.filter((w) => w !== alias).sort((a, b) => newer(a, b, name));
    const pick = alias ?? pinned[0];
    if (!pick) continue;
    out.push({ name, pick, versions: alias ? [alias, ...pinned] : pinned });
  }
  return out;
}

/* ------------------------------------------------------------------ a row of words */

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

/** A few short choices as words: a radiogroup (arrow keys move, Enter or Space picks). */
function WordRow({ label, words, value, onPick }: { label: string; words: Word[]; value: string; onPick: (w: Word) => void }) {
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
    const moves: Record<string, number> = { ArrowRight: i + 1, ArrowDown: i + 1, ArrowLeft: i - 1 + n, ArrowUp: i - 1 + n, Home: 0, End: n - 1 };
    const j = moves[e.key];
    if (j === undefined) return;
    e.preventDefault();
    radios[j % n]?.focus();
  };

  return (
    <div
      ref={group}
      role="radiogroup"
      aria-label={label}
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
            aria-label={w.full !== w.text && w.full.toLowerCase().includes(w.text.toLowerCase()) ? w.full : undefined}
            title={w.hint ? `${w.full}: ${w.hint}` : w.full}
            tabIndex={w.id === stop ? 0 : -1}
            className="rs-word"
            onFocus={() => setFocus(w.id)}
            onClick={() => onPick(w)}
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
  );
}

/* ------------------------------------------------------------------ a scale */

/**
 * An ordered setting as one track: the choice's name and what it does above, the stops below.
 * Tap a stop or slide along the track (the name follows the finger; it applies where it's let go);
 * arrow keys, Home and End move a stop at a time.
 */
function Scale({ label, words, value, armed, onPick }: { label: string; words: Word[]; value: string; armed?: string; onPick: (w: Word) => void }) {
  const rail = React.useRef<HTMLDivElement>(null);
  const [drag, setDrag] = React.useState<number | null>(null);
  const n = words.length;
  const at = Math.max(
    0,
    words.findIndex((w) => w.id === (armed ?? value)),
  );
  const shown = drag ?? at;
  const w = words[shown];
  const pct = (i: number) => (n > 1 ? (i / (n - 1)) * 100 : 0);
  const indexAt = (x: number) => {
    const r = rail.current?.getBoundingClientRect();
    if (!r || r.width <= 0 || n < 2) return at;
    return Math.min(n - 1, Math.max(0, Math.round(((x - r.left) / r.width) * (n - 1))));
  };
  const commit = (i: number) => {
    const to = words[i];
    if (to) onPick(to);
  };
  const first = words[0];
  const last = words[n - 1];

  return (
    <div className="rs-scale">
      <p className="rs-now" data-tone={w?.dangerous ? "warn" : undefined}>
        <span className="rs-now-name">{w?.text}</span>
        {w?.hint && <span className="rs-now-hint">{w.hint}</span>}
      </p>
      {n > 1 && (
        <div
          role="slider"
          tabIndex={0}
          aria-label={label}
          aria-valuemin={0}
          aria-valuemax={n - 1}
          aria-valuenow={at}
          aria-valuetext={w?.full}
          className="rs-track"
          data-dragging={drag != null ? "" : undefined}
          onPointerDown={(e) => {
            if (e.button !== 0) return;
            e.currentTarget.setPointerCapture(e.pointerId);
            setDrag(indexAt(e.clientX));
          }}
          onPointerMove={(e) => {
            if (drag != null) setDrag(indexAt(e.clientX));
          }}
          onPointerUp={(e) => {
            if (drag == null) return;
            setDrag(null);
            commit(indexAt(e.clientX));
          }}
          onPointerCancel={() => setDrag(null)}
          onKeyDown={(e) => {
            const moves: Record<string, number> = { ArrowRight: at + 1, ArrowUp: at + 1, ArrowLeft: at - 1, ArrowDown: at - 1, Home: 0, End: n - 1 };
            const to = moves[e.key];
            if (to === undefined) return;
            e.preventDefault();
            if (to >= 0 && to < n && to !== at) commit(to);
          }}
        >
          <div ref={rail} className="rs-rail">
            <i className="rs-fill" style={{ width: `${pct(shown)}%` }} aria-hidden="true" />
            {words.map((x, i) => (
              <i key={x.id} className="rs-tick" data-on={i <= shown ? "" : undefined} data-warn={x.dangerous ? "" : undefined} style={{ left: `${pct(i)}%` }} aria-hidden="true" />
            ))}
            <i className="rs-knob" data-warn={w?.dangerous ? "" : undefined} style={{ left: `${pct(shown)}%` }} aria-hidden="true" />
          </div>
        </div>
      )}
      {n > 1 && first && last && (
        <p className="rs-ends" aria-hidden="true">
          <span>{first.text}</span>
          <span>{last.text}</span>
        </p>
      )}
    </div>
  );
}

/* ------------------------------------------------------------------ the model */

/** The model as its family, with the family's versions one tap further. */
function ModelChoice({ words, value, onPick }: { words: Word[]; value: string; onPick: (w: Word) => void }) {
  const [versions, setVersions] = React.useState(false);
  const fams = families(words);
  const chosen = words.find((w) => w.id === value);
  // Few models, or none sharing a family: just the models.
  if (fams.length === words.length || words.length <= 4) {
    return <WordRow label="Model" words={words} value={value} onPick={onPick} />;
  }
  const current = fams.find((f) => f.versions.some((v) => v.id === value));
  const famWords: Word[] = fams.map((f) => ({ id: `family:${f.name}`, text: f.name, full: f.name, hint: f.pick.hint }));
  const own = current?.versions ?? [];
  return (
    <div className="rs-model">
      <WordRow
        label="Model family"
        words={famWords}
        value={current ? `family:${current.name}` : ""}
        onPick={(fw) => {
          const f = fams.find((x) => `family:${x.name}` === fw.id);
          if (f && f !== current) {
            setVersions(false);
            onPick(f.pick);
          }
        }}
      />
      <p className="rs-now">
        <span className="rs-now-name">{chosen?.text ?? value}</span>
        {chosen?.hint && <span className="rs-now-hint">{chosen.hint}</span>}
        {own.length > 1 && (
          <button type="button" className="rs-more" aria-expanded={versions} onClick={() => setVersions((v) => !v)}>
            {versions ? "fewer" : `${own.length - 1} other version${own.length === 2 ? "" : "s"}`}
          </button>
        )}
      </p>
      {versions && current && own.length > 1 && (
        <WordRow label={`${current.name} versions`} words={own.map((v) => ({ ...v, text: versionText(v, current.name) }))} value={value} onPick={onPick} />
      )}
    </div>
  );
}

/* ------------------------------------------------------------------ the line */

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
 * The head's line of how an agent runs. Each value opens just its own setting, in place; a mode
 * that turns checks off waits for "Turn them off"; a change can be undone for a few seconds.
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
  const [open, setOpen] = React.useState<Kind | null>(null);
  const [change, setChange] = React.useState<Change | null>(null);
  /** A mode that turns checks off, waiting for "Turn them off". */
  const [armed, setArmed] = React.useState<{ kind: Kind; id: string } | null>(null);
  /** Counts changes made since the setting opened; the fold timer restarts on each. */
  const [tick, setTick] = React.useState(0);
  const [hover, setHover] = React.useState(false);
  const [keyboard, setKeyboard] = React.useState(false);
  const [said, setSaid] = React.useState("");
  const root = React.useRef<HTMLDivElement>(null);
  const values = React.useRef<Partial<Record<Kind, HTMLButtonElement | null>>>({});
  /** Where focus goes after the next render, when the element that had it goes away. */
  const refocus = React.useRef<Kind | null>(null);
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
  const editable = (k: Kind) => Boolean(sets[k].pick && sets[k].count > 0);
  const nextTurn = (k: Kind) => Boolean(running && appliesNextTurn?.[k]);
  const waiting = armed && sets[armed.kind].words.some((w) => w.id === armed.id) ? armed : null;

  // Undo is offered for a few seconds, longer while a keyboard or mouse is on it.
  React.useEffect(() => {
    if (!change || held) return;
    const t = window.setTimeout(() => setChange(null), UNDO_MS);
    return () => window.clearTimeout(t);
  }, [change, held]);

  // The setting folds away a little after a change, unless keyboard focus or a mouse is in it, or
  // a confirmation is waiting.
  React.useEffect(() => {
    if (!open || tick === 0 || held || waiting) return;
    const t = window.setTimeout(() => {
      if (root.current?.contains(document.activeElement)) refocus.current = open;
      setOpen(null);
    }, FOLD_MS);
    return () => window.clearTimeout(t);
  }, [open, tick, held, waiting]);

  // A waiting confirmation stops waiting when anything other than its track or button is tapped or focused.
  React.useEffect(() => {
    if (!waiting) return;
    const away = (e: Event) => {
      const keep = [root.current?.querySelector(".rs-confirm-line"), root.current?.querySelector(".rs-track")];
      if (e.target instanceof Node && keep.some((el) => el?.contains(e.target as Node))) return;
      setArmed(null);
    };
    document.addEventListener("pointerdown", away, true);
    document.addEventListener("focusin", away, true);
    return () => {
      document.removeEventListener("pointerdown", away, true);
      document.removeEventListener("focusin", away, true);
    };
  }, [waiting]);

  React.useLayoutEffect(() => {
    const to = refocus.current;
    if (!to) return;
    refocus.current = null;
    values.current[to]?.focus({ preventScroll: true });
  });

  const apply = (k: Kind, to: string) => {
    const pick = sets[k].pick;
    const prev = shown(k);
    if (!pick || to === prev) return;
    pick(to);
    setChange({ kind: k, base: actual[k], prev, to, undoable: Boolean(prev) });
    setArmed(null);
    setTick((t) => t + 1);
    setSaid(`${KEY[k]}: ${nameOf(k, to)}${nextTurn(k) ? ", from the next turn" : ""}`);
  };

  const choose = (k: Kind, w: Word) => {
    if (w.id === shown(k)) {
      setArmed(null);
      return;
    }
    if (w.dangerous) {
      setArmed({ kind: k, id: w.id });
      setSaid(`${w.full}: ${CONFIRM}`);
      return;
    }
    apply(k, w.id);
  };

  const undo = () => {
    if (!change?.undoable) return;
    const { kind: k, prev, to } = change;
    sets[k].pick?.(prev);
    setChange({ kind: k, base: actual[k], prev: to, to: prev, undoable: false });
    setArmed(null);
    setTick((t) => t + 1);
    setSaid(`${KEY[k]}: back to ${nameOf(k, prev)}`);
  };

  const toggle = (k: Kind) => {
    setArmed(null);
    if (open === k) {
      setOpen(null);
      return;
    }
    setOpen(k);
    setTick(0);
  };

  let parts = KINDS.map((k) => ({ k, text: shown(k) ? nameOf(k, shown(k)) : "", empty: false })).filter((p) => p.text);
  if (parts.length === 0) parts = KINDS.filter(editable).map((k) => ({ k, text: KEY[k], empty: true }));
  if (parts.length === 0) return null;

  const undoable = change?.undoable ? change : null;
  const panel = (k: Kind) => {
    const s = sets[k];
    const pick = (w: Word) => choose(k, w);
    const after = nextTurn(k) ? <p className="rs-desc">From the next turn</p> : null;
    if (k === "model") {
      return (
        <>
          <ModelChoice words={s.words} value={shown(k)} onPick={pick} />
          {after}
        </>
      );
    }
    const w = waiting?.kind === k ? s.words.find((x) => x.id === waiting.id) : undefined;
    return (
      <>
        <Scale label={KEY[k]} words={s.words} value={shown(k)} armed={w?.id} onPick={pick} />
        {w && (
          <p className="rs-desc rs-confirm-line" data-tone="warn">
            {CONFIRM}{" "}
            <button type="button" className="rs-confirm" onClick={() => apply(k, w.id)}>
              Turn them off
            </button>
          </p>
        )}
        {after}
      </>
    );
  };

  return (
    <div
      ref={root}
      className={cn("rs-tuner", className)}
      data-open={open ? "" : undefined}
      onKeyDown={(e) => {
        if (e.key !== "Escape" || !open) return;
        e.preventDefault();
        e.stopPropagation();
        if (waiting) {
          setArmed(null);
          return;
        }
        if (root.current?.contains(document.activeElement)) refocus.current = open;
        setOpen(null);
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
        <p className="rs-line">
          {parts.map((p, i) => (
            <React.Fragment key={p.k}>
              {i > 0 && (
                <span className="rs-sep" aria-hidden="true">
                  {" · "}
                </span>
              )}
              {editable(p.k) ? (
                <button
                  ref={(el) => {
                    values.current[p.k] = el;
                  }}
                  type="button"
                  className="rs-v"
                  data-empty={p.empty ? "" : undefined}
                  aria-expanded={open === p.k}
                  aria-controls={open === p.k ? panelId : undefined}
                  aria-label={`${KEY[p.k]}: ${p.empty ? "choose" : nameOf(p.k, shown(p.k))}`}
                  onClick={() => toggle(p.k)}
                >
                  {p.text}
                </button>
              ) : (
                <span className="rs-v" data-fixed="">
                  {p.text}
                </span>
              )}
            </React.Fragment>
          ))}
        </p>
        {undoable && (
          <span className="rs-after">
            <span className="rs-dot" aria-hidden="true">
              ·
            </span>
            <button type="button" className="rs-undo" aria-label={`Undo: back to ${nameOf(undoable.kind, undoable.prev)}`} onClick={undo}>
              Undo
            </button>
          </span>
        )}
      </div>
      {open && (
        <div id={panelId} className="rs-panel" role="group" aria-label={KEY[open]} data-kind={open}>
          {panel(open)}
        </div>
      )}
      <span className="rs-sr" role="status">
        {said}
      </span>
    </div>
  );
}

/* ------------------------------------------------------------------ one message */

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
 * A row for the composer: THIS MESSAGE  SAME · MAX EFFORT · ANOTHER MODEL. Another model opens the
 * model families (and their versions) under it. The composer keeps the choice, sends it with the
 * next message and clears it after. Renders nothing when there's nothing to offer.
 */
export function SendWith({ models, model, modelLabel = same, efforts, effort, value, onChange, className }: SendWithProps) {
  const [picking, setPicking] = React.useState(false);
  const others = models.filter((m) => m.id !== model);
  const top = efforts[efforts.length - 1];
  const effortName = (id: string) => efforts.find((e) => e.id === id)?.label || capital(id);
  const words: Word[] = [{ id: "same", text: "Same", full: "Same as the agent" }];
  if (top && top.id !== effort) words.push({ id: `effort:${top.id}`, text: `${effortName(top.id)} effort`, full: `${effortName(top.id)} effort` });
  if (value.effort && value.effort !== top?.id) words.push({ id: `effort:${value.effort}`, text: `${effortName(value.effort)} effort`, full: `${effortName(value.effort)} effort` });
  const chosenModel = value.model ? modelWord(models.find((m) => m.id === value.model) ?? { id: value.model }, modelLabel) : null;
  if (others.length > 0) words.push({ id: "model", text: chosenModel ? chosenModel.text : "Another model", full: chosenModel ? chosenModel.full : "Another model" });
  if (words.length < 2) return null;

  const current = value.effort ? `effort:${value.effort}` : value.model || picking ? "model" : "same";
  const choices = others.map((m) => modelWord(m, modelLabel));
  return (
    <div className={cn("rs-tuner rs-once", className)}>
      <div className="rs-set">
        <span className="rs-key">This message</span>
        <div className="rs-body">
          <WordRow
            label="This message"
            words={words}
            value={current}
            onPick={(w) => {
              if (w.id === "model") {
                setPicking(true);
                return;
              }
              setPicking(false);
              onChange(w.id.startsWith("effort:") ? { effort: w.id.slice(7) } : {});
            }}
          />
        </div>
      </div>
      {(picking || value.model) && choices.length > 0 && (
        <div className="rs-set">
          <span className="rs-key" aria-hidden="true" />
          <div className="rs-body">
            <ModelChoice words={choices} value={value.model ?? ""} onPick={(w) => onChange({ model: w.id })} />
          </div>
        </div>
      )}
    </div>
  );
}
