/**
 * How an agent runs (its permissions, model and effort) as the line the head already shows,
 * ASK · OPUS 5.5 · MAX. Each value is its own control: tap one and just that setting opens in
 * place, with the control that suits it.
 *
 * - Permissions and effort are scales (from "plans only" to "asks nothing", from low to max), so
 *   each is one track: tap a stop or slide to it. A mode that turns checks off waits for an
 *   explicit "Turn them off".
 * - The model is a family (Opus, Fable, Sonnet, Haiku); its other versions are one tap further.
 *   The other harnesses' models on the agent's machine follow, by harness: choosing one goes on
 *   there (at the end of the turn under way), the conversation carried over.
 *
 * A choice applies at once, Undo waits beside the turn timer for a few seconds, and the setting folds
 * away shortly after. Nothing pops up or covers the conversation.
 *
 * Where the model goes faster (Claude Code's fast mode, Codex's tiers, Devin's speed), a bolt follows
 * effort: a click moves to the model's next speed, and from its fastest back to standard.
 *
 * `SendWith` is the composer's row for a single message: the same, max effort, or another model.
 */
import * as React from "react";
import type { ControlOption, ModelOption, ModeOption } from "@/api/harness";
import { cn } from "@/lib/utils";
import { useListener } from "./dom";
import { boltLabel, boltState, lit, nextSpeed, speedName, type SpeedLine } from "./speed";
import "./run-settings.css";

type Kind = "mode" | "model" | "effort";

/** One choice. */
export interface Word {
  id: string;
  /** What it shows, kept short ("Edits", "Opus 5.5"). */
  text: string;
  /** Its full name ("Accept edits"), for assistive tech and the tooltip. */
  full: string;
  /** What it does: shown under the scale while it's the one chosen (or slid to). */
  hint?: string;
  /** Turns checks off, so it takes an explicit confirmation. */
  dangerous?: boolean;
  /** A model of another harness than the agent's (its id): choosing it switches there. */
  kind?: string;
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
    // It never asks, refusing whatever isn't already allowed: by read-only, not past auto.
    if (/dontask|donotask|neverask|allowedonly/.test(s)) return 0.5;
    if (/^plan|readonly/.test(s)) return 0;
    if (/acceptedit|autoedit|editsok|allowedit/.test(s)) return 2;
    if (/^auto/.test(s)) return 3;
    if (/^(ask|default|untrusted|onrequest|manual|normal)/.test(s)) return 1;
  }
  return undefined;
}

/**
 * Read-only first, then the mode that refuses what isn't allowed, ask, accept edits, auto. Modes
 * we don't recognise keep their order after those; any that turn checks off come last.
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

/** Another harness's model as its word's id, its own (two harnesses may list the same model); `model` "": its own default, for one that lists none yet. */
export const modelIn = (kind: string, model: string) => `kiln-in:${kind}:${model}`;
/** The model a word stands for ("" another harness's own default). */
const modelOfWord = (id: string) => /^kiln-in:[^:]*:([\s\S]*)$/.exec(id)?.[1] ?? id;

/** A model reads as `modelLabel` makes it ("Opus 5.5") when that knows the id, else as the harness names it. */
function modelWord(m: { id: string; label?: string; description?: string }, pretty: (id: string) => string): Word {
  const p = pretty(m.id);
  return { id: m.id, text: p !== m.id ? p : m.label || m.id, full: m.label || p, hint: m.description?.trim() || undefined };
}

function modelWords(models: ModelOption[], current: string, pretty: (id: string) => string): Word[] {
  const list = models.map((m) => modelWord(m, pretty));
  // The agent's own model, when the harness doesn't list it, goes last: the families keep the harness's order. Another
  // harness's (a switch waiting) reads as its model, or "Default".
  return current && !list.some((w) => w.id === current) ? [...list, { ...modelWord({ id: modelOfWord(current) || "default" }, pretty), id: current }] : list;
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
  return (text.match(/\d+(?:\.\d+)*/)?.[0] ?? "").split(".").filter(Boolean).map(Number);
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

/**
 * The ref callback for a row's underline: keeps it under the checked word. It places it when the
 * row appears, and again whenever the row changes (another word checked, other words) or resizes.
 * Once placed it glides from word to word (CSS: transform only).
 */
function glide(b: HTMLElement | null) {
  const g = b?.parentElement;
  if (!b || !g) return;
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
  // Another word checked, or other words: React changes the row, and the underline follows before it's drawn.
  const changes = new MutationObserver(place);
  changes.observe(g, { subtree: true, childList: true, characterData: true, attributeFilter: ["aria-checked"] });
  void document.fonts?.ready.then(place);
  return () => {
    live = false;
    cancelAnimationFrame(frame);
    watch.disconnect();
    changes.disconnect();
  };
}

/** A few short choices as words: a radiogroup (arrow keys move, Enter or Space picks). */
export function WordRow({ label, words, value, onPick }: { label: string; words: Word[]; value: string; onPick: (w: Word) => void }) {
  const [focus, setFocus] = React.useState<string | null>(null);
  // One stop in the tab order: the word with focus, else the current one, else the first.
  const stop = [focus, value, words[0]?.id].find((id) => id != null && words.some((w) => w.id === id));

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
      <i ref={glide} className="rs-glide" aria-hidden="true" />
    </div>
  );
}

/* ------------------------------------------------------------------ a scale */

/**
 * An ordered setting as one track: the choice's name and what it does above, the stops below.
 * Tap a stop or slide along the track (the name follows the finger; it applies where it's let go);
 * arrow keys, Home and End move a stop at a time.
 */
export function Scale({ label, words, value, armed, onPick }: { label: string; words: Word[]; value: string; armed?: string; onPick: (w: Word) => void }) {
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
  // Few models, or none sharing a family: just the models, the chosen one said under them.
  if (fams.length === words.length || words.length <= 4) {
    return (
      <div className="rs-model">
        <WordRow label="Model" words={words} value={value} onPick={onPick} />
        {chosen && (
          <p className="rs-now">
            <span className="rs-now-name">{chosen.text}</span>
            {chosen.hint && <span className="rs-now-hint">{chosen.hint}</span>}
          </p>
        )}
      </div>
    );
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
      {/* Only where the chosen model is: another harness's models don't name it. */}
      {chosen && (
        <p className="rs-now">
          <span className="rs-now-name">{chosen.text}</span>
          {chosen.hint && <span className="rs-now-hint">{chosen.hint}</span>}
          {own.length > 1 && (
            <button type="button" className="rs-more" aria-expanded={versions} onClick={() => setVersions((v) => !v)}>
              {versions ? "fewer" : `${own.length - 1} other version${own.length === 2 ? "" : "s"}`}
            </button>
          )}
        </p>
      )}
      {versions && current && own.length > 1 && <WordRow label={`${current.name} versions`} words={own.map((v) => ({ ...v, text: versionText(v, current.name) }))} value={value} onPick={onPick} />}
    </div>
  );
}

/* ------------------------------------------------------------------ the speed */

/** The bolt, drawn as kiln's own small glyphs are: a 16 grid, a 1.4 stroke, round joins. */
const BOLT = "M8.75 1.5 2.5 9.25H8l-.75 5.25 6.25-7.75H8z";

/**
 * The agent's speed, after effort: its outline at standard; filled, the prism running through it,
 * when faster (a bolt for each step up); grey and still while fast mode cools down; only sketched
 * when the account can't go faster. It's as wide as the model's fastest speed draws it, so the
 * bolts it fills never move what follows.
 */
function SpeedBolt({ line, onPick }: { line: SpeedLine; onPick?: () => void }) {
  const state = boltState(line);
  const on = state === "on" || state === "cooldown" ? lit(line.speeds, line.speed) : 0;
  const label = boltLabel(line);
  const style = { "--n": line.speeds.length } as React.CSSProperties;
  const glyph = (
    <>
      <svg className="rs-bolt-line" viewBox="0 0 16 16" aria-hidden="true">
        <path d={BOLT} />
      </svg>
      {line.speeds.map((s, i) => (
        <span key={s.id} className="rs-bolt-fill" data-on={i < on ? "" : undefined} style={{ "--i": i } as React.CSSProperties} aria-hidden="true">
          <i className="rs-bolt-holo" />
          <i className="rs-bolt-glint" />
        </span>
      ))}
    </>
  );
  if (!onPick) {
    return (
      <span className="rs-bolt" data-state={state} style={style} role="img" aria-label={label} title={label}>
        {glyph}
      </span>
    );
  }
  return (
    <button
      type="button"
      className="rs-bolt"
      data-state={state}
      style={style}
      aria-label={label}
      title={label}
      aria-disabled={state === "blocked" || undefined}
      onClick={state === "blocked" ? undefined : onPick}
    >
      {glyph}
    </button>
  );
}

/* ------------------------------------------------------------------ the line */

/** Another harness on the agent's machine, and its models. */
export interface Elsewhere {
  kind: string;
  name: string;
  /** As its sessions there last listed them; none listed yet, it's chosen with its own default. */
  models: ModelOption[];
}

export interface RunSettingsProps {
  modes: ModeOption[];
  mode: string;
  /** Leave out when the harness can't change permissions: the value then shows as plain text. */
  onMode?: (id: string) => void;
  /** A running turn can be interrupted and continued with the current permission preset. */
  modeCanReapply?: boolean;
  models: ModelOption[];
  model: string;
  /** With the harness the model is in: the agent's own (`harness`) or one of `elsewhere`. */
  onModel?: (id: string, kind?: string) => void;
  /** The harness the agent runs in, and the others whose models it can go on with (its conversation carried over). */
  harness?: string;
  elsewhere?: Elsewhere[];
  /** How a model id reads ("claude-opus-5-5" → "Opus 5.5"). */
  modelLabel?: (id: string) => string;
  efforts: ControlOption[];
  effort: string;
  onEffort?: (id: string) => void;
  /** The speeds faster than standard the model in use runs at, slowest first: the bolt after effort (none: no bolt). */
  speeds?: ControlOption[];
  /** The speed it runs at: "" standard. */
  speed?: string;
  /** Leave out when the harness can't change its speed: the bolt then only shows it. */
  onSpeed?: (id: string) => void;
  /** Why it can't go faster now, as its harness said: the bolt is only sketched, and says this. */
  speedBlocked?: string;
  /** Fast mode's own rate limit is cooling down: the bolt is filled, grey and still. */
  speedCooldown?: boolean;
  /** Which settings the harness takes up at the next turn rather than at once (said only while it works). */
  appliesNextTurn?: { mode?: boolean; model?: boolean; effort?: boolean };
  running?: boolean;
  className?: string;
  /** Related controls on the same line, divided from the run settings. */
  trailing?: React.ReactNode;
  /** What choosing another harness's model does, said under its models (by default: the agent goes on there, its conversation carried over). */
  switchWords?: (name: string) => React.ReactNode;
}

/** A change just made: what the line shows until the agent reports otherwise, and what Undo restores. */
interface Change {
  kind: Kind;
  /** The agent's own value when the change was made. The line shows `to` while the agent still reports this. */
  base: string;
  /** What Undo puts back. */
  prev: string;
  to: string;
  /** For a model: the harnesses of `prev` and `to`. */
  prevKind?: string;
  toKind?: string;
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
  modeCanReapply = false,
  models,
  model,
  onModel,
  harness,
  elsewhere = [],
  modelLabel = same,
  efforts,
  effort,
  onEffort,
  speeds = [],
  speed = "",
  onSpeed,
  speedBlocked,
  speedCooldown,
  appliesNextTurn,
  running = false,
  className,
  trailing,
  switchWords,
}: RunSettingsProps) {
  const [open, setOpen] = React.useState<Kind | null>(null);
  const [change, setChange] = React.useState<Change | null>(null);
  /** The speed just clicked to: what the bolt shows until the agent has had time to say (clicks in a row go on from it). */
  const [speedPick, setSpeedPick] = React.useState<string | null>(null);
  /** A mode that turns checks off, waiting for "Turn them off". */
  const [armed, setArmed] = React.useState<{ kind: Kind; id: string } | null>(null);
  /** Counts changes made since the setting opened; the fold timer restarts on each. */
  const [tick, setTick] = React.useState(0);
  const [hover, setHover] = React.useState(false);
  const [keyboard, setKeyboard] = React.useState(false);
  const [said, setSaid] = React.useState("");
  const root = React.useRef<HTMLDivElement>(null);
  const values = React.useRef<Partial<Record<Kind, HTMLButtonElement | null>>>({});
  const panelId = React.useId();
  const held = hover || keyboard;

  const actual: Record<Kind, string> = { mode, model, effort };
  const shown = (k: Kind) => (change?.kind === k && actual[k] === change.base ? change.to : actual[k]);
  // Codex's Custom label describes separate controls; it isn't a preset that can be selected.
  const isPreset = (id: string) => modes.some((m) => m.id === id) && !(harness === "codex" && id === "custom");
  const availableModes = modes.filter((m) => isPreset(m.id) || m.id === shown("mode"));
  // The other harnesses' models, by harness (they switch the agent there); one that lists none yet, its own default.
  const groups = onModel
    ? elsewhere.map((h) => ({
        ...h,
        words:
          h.models.length > 0
            ? h.models.map((m) => ({ ...modelWord(m, modelLabel), id: modelIn(h.kind, m.id), kind: h.kind }))
            : [{ id: modelIn(h.kind, ""), text: "Default", full: "Its default model", kind: h.kind }],
      }))
    : [];
  const others = groups.flatMap((g) => g.words);
  // The agent's own models (its current one among them, unless that is another harness's: a switch waiting).
  const own = modelWords(models, others.some((w) => w.id === shown("model")) ? "" : shown("model"), modelLabel);
  const sets: Record<Kind, { words: Word[]; count: number; pick?: (id: string) => void }> = {
    mode: { words: modeWords(availableModes, shown("mode")), count: availableModes.length, pick: onMode },
    model: { words: [...own, ...others], count: models.length + others.length, pick: onModel },
    effort: { words: effortWords(efforts, shown("effort")), count: efforts.length, pick: onEffort },
  };
  // Another harness's model goes there; the agent's own stays (or comes back from a switch waiting).
  const kindOf = (id: string) => others.find((w) => w.id === id)?.kind;
  const nameOf = (k: Kind, id: string) => sets[k].words.find((w) => w.id === id)?.full ?? id;
  const editable = (k: Kind) => Boolean(sets[k].pick && sets[k].count > 0);
  const nextTurn = (k: Kind) => Boolean(running && appliesNextTurn?.[k]);
  const waiting = armed && sets[armed.kind].words.some((w) => w.id === armed.id) ? armed : null;

  /** Folds the setting away; focus that was in it goes to its value on the line (which stays). */
  const fold = React.useCallback((k: Kind) => {
    if (root.current?.contains(document.activeElement)) values.current[k]?.focus({ preventScroll: true });
    setOpen(null);
  }, []);

  // Undo is offered for a few seconds, longer while a keyboard or mouse is on it. The top line shows
  // the change meanwhile: its timer, set again whenever either changes.
  const lasting = React.useCallback(
    (top: HTMLDivElement | null) => {
      if (!top || !change || held) return;
      const t = window.setTimeout(() => setChange(null), UNDO_MS);
      return () => window.clearTimeout(t);
    },
    [change, held],
  );

  // A speed just clicked to shows until the agent has had time to say what it runs at: its own
  // timer, on the line (which stays while the bolt comes and goes with the model), set again by each click.
  const settling = React.useCallback(
    (line: HTMLParagraphElement | null) => {
      if (!line || speedPick === null) return;
      const t = window.setTimeout(() => setSpeedPick(null), UNDO_MS);
      return () => window.clearTimeout(t);
    },
    [speedPick],
  );
  const speedLine: SpeedLine = { speeds, speed: speedPick ?? speed, blocked: speedBlocked, cooldown: speedCooldown };
  const pickSpeed = () => {
    const to = nextSpeed(speeds, speedLine.speed);
    onSpeed?.(to);
    setSpeedPick(to);
    setSaid(`Speed: ${speedName(speeds, to)}${running ? ", from the next turn" : ""}`);
  };

  // The setting folds away a little after a change, unless keyboard focus or a mouse is in it, or
  // a confirmation is waiting: the open setting's own timer, set again whenever one of those changes.
  const folding = React.useCallback(
    (panel: HTMLDivElement | null) => {
      if (!panel || !open || tick === 0 || held || waiting) return;
      const t = window.setTimeout(() => fold(open), FOLD_MS);
      return () => window.clearTimeout(t);
    },
    [open, tick, held, waiting, fold],
  );

  // A waiting confirmation stops waiting when anything other than its track or button is tapped or focused.
  const away = (e: Event) => {
    const keep = [root.current?.querySelector(".rs-confirm-line"), root.current?.querySelector(".rs-track")];
    if (e.target instanceof Node && keep.some((el) => el?.contains(e.target as Node))) return;
    setArmed(null);
  };
  useListener(document, "pointerdown", away, { on: Boolean(waiting), capture: true });
  useListener(document, "focusin", away, { on: Boolean(waiting), capture: true });

  const apply = (k: Kind, to: string) => {
    const pick = sets[k].pick;
    const prev = shown(k);
    const reapply = k === "mode" && modeCanReapply && isPreset(to) && to === prev;
    if (!pick || (to === prev && !reapply) || (k === "mode" && !isPreset(to))) return;
    const [prevKind, toKind] = k === "model" ? [kindOf(prev), kindOf(to)] : [undefined, undefined];
    if (k === "model" && onModel) onModel(modelOfWord(to), toKind);
    else pick(to);
    setChange({ kind: k, base: actual[k], prev, to, undoable: Boolean(prev) && !reapply && (k !== "mode" || isPreset(prev)), prevKind, toKind });
    setArmed(null);
    setTick((t) => t + 1);
    const there = groups.find((g) => g.kind === toKind)?.name;
    setSaid(
      `${KEY[k]}: ${nameOf(k, to)}${there ? `, in ${there}: the conversation carries over${running ? " at the end of this turn" : ""}` : nextTurn(k) ? ", from the next turn" : reapply ? ", applying now" : ""}`,
    );
  };

  const choose = (k: Kind, w: Word, reapply = false) => {
    if ((w.id === shown(k) && !reapply) || (k === "mode" && !isPreset(w.id))) {
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
    const { kind: k, prev, to, prevKind, toKind } = change;
    // A switch undone goes back to the harness the agent was in (or doesn't happen).
    if (k === "model" && onModel) onModel(modelOfWord(prev), prevKind);
    else sets[k].pick?.(prev);
    setChange({ kind: k, base: actual[k], prev: to, to: prev, undoable: false, prevKind: toKind, toKind: prevKind });
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

  // Every setting keeps its column, even when a harness has no value for it yet.
  const parts = KINDS.map((k) => ({
    k,
    text: shown(k) ? (sets[k].words.find((w) => w.id === shown(k))?.text ?? shown(k)) : editable(k) ? KEY[k] : "—",
    empty: !shown(k),
  }));

  const undoable = change?.undoable ? change : null;
  const panel = (k: Kind) => {
    const s = sets[k];
    const pick = (w: Word) => choose(k, w);
    const after = nextTurn(k) ? <p className="rs-desc">From the next turn</p> : null;
    if (k === "model") {
      return (
        <>
          {own.length > 0 && <ModelChoice words={own} value={shown(k)} onPick={pick} />}
          {after}
          {groups.map((g) => (
            <div key={g.kind} className="rs-elsewhere" role="group" aria-label={`${g.name} models`}>
              <p className="rs-elsewhere-name">{g.name}</p>
              <ModelChoice words={g.words} value={shown(k)} onPick={pick} />
              <p className="rs-desc">
                {switchWords ? (
                  switchWords(g.name)
                ) : (
                  <>
                    Switches to {g.name}
                    {running ? " at the end of this turn" : ""} — the conversation carries over
                  </>
                )}
              </p>
            </div>
          ))}
        </>
      );
    }
    const w = waiting?.kind === k ? s.words.find((x) => x.id === waiting.id) : undefined;
    const current = k === "mode" && modeCanReapply && isPreset(shown(k)) ? s.words.find((x) => x.id === shown(k)) : undefined;
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
        {k === "mode" && modeCanReapply && (
          <p className="rs-desc">
            Applying permissions interrupts this turn, then continues it.{" "}
            {current && !w && (
              <button type="button" className="rs-more" aria-label={`Apply ${current.full} now`} onClick={() => choose(k, current, true)}>
                Apply now
              </button>
            )}
          </p>
        )}
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
        fold(open);
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
      <div ref={lasting} className="rs-top">
        <p ref={settling} className="rs-line">
          <span className="rs-values">
            {parts.map((p, i) => (
              <React.Fragment key={p.k}>
                {i > 0 && (
                  <span className="rs-sep" aria-hidden="true">
                    ·
                  </span>
                )}
                {editable(p.k) ? (
                  <button
                    ref={(el) => {
                      values.current[p.k] = el;
                    }}
                    type="button"
                    className="rs-v"
                    data-kind={p.k}
                    data-empty={p.empty ? "" : undefined}
                    aria-expanded={open === p.k}
                    aria-controls={open === p.k ? panelId : undefined}
                    aria-label={`${KEY[p.k]}: ${p.empty ? "choose" : nameOf(p.k, shown(p.k))}`}
                    title={p.empty ? KEY[p.k] : nameOf(p.k, shown(p.k))}
                    onClick={() => toggle(p.k)}
                  >
                    <span className="rs-value-text">{p.text}</span>
                  </button>
                ) : (
                  <span className="rs-v" data-kind={p.k} data-fixed="" title={`${KEY[p.k]}: ${p.empty ? "unavailable" : nameOf(p.k, shown(p.k))}`}>
                    {p.text}
                  </span>
                )}
              </React.Fragment>
            ))}
          </span>
          {speeds.length > 0 && <SpeedBolt line={speedLine} onPick={onSpeed ? pickSpeed : undefined} />}
        </p>
        <div className="rs-extra">
          {trailing && (
            <>
              <span className="rs-divider" aria-hidden="true">
                |
              </span>
              {trailing}
            </>
          )}
        </div>
        {undoable && (
          <span className="rs-after">
            <button type="button" className="rs-undo" aria-label={`Undo: back to ${nameOf(undoable.kind, undoable.prev)}`} onClick={undo}>
              Undo
            </button>
          </span>
        )}
      </div>
      {open && (
        <div ref={folding} id={panelId} className="rs-panel" role="group" aria-label={KEY[open]} data-kind={open}>
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
