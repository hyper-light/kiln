/**
 * Where an agent works, as one line: IN  ~ / Projects / kiln. Tap it, or any folder on it, and it
 * opens in place underneath; what follows moves down, and nothing pops up or covers anything. Open,
 * it has the path to type into, the folders inside as words (most recently changed first, after
 * "↑ .." to go up) and the folders used before. Tapping a folder goes into it and applies it at
 * once. Typing narrows the folders, Tab or → completes, Enter applies what's typed and folds, Esc
 * puts back what it was when it opened. Tapping anywhere else folds it, keeping what's applied.
 */
import * as React from "react";
import { flushSync } from "react-dom";
import "./inline-path.css";
import { ApiError, machineDirs } from "@/api/client";
import type { DirEntry, DirListing } from "@/api/types";
import { relativeTime } from "@/lib/format";
import { cn } from "@/lib/utils";
import { useClock, useListener } from "./dom";
import { Optic } from "./signals";

export interface InlinePathProps {
  /** The machine: "local" (the kiln server), "agent:<name>" or a machine's name. */
  target: string;
  /** The folder applied now; `~` is home. */
  value: string;
  onChange: (path: string) => void;
  /** Folders used before on this machine, most recent first. */
  recent?: string[];
  /** The eyebrow before the path. */
  label?: string;
  className?: string;
}

/** Folder words shown before "+N more" (typing shows every match); fewer on a phone. */
const FIRST = 24;
const FIRST_NARROW = 12;
/** A listing is reused for this long, so going back and forth asks again for nothing. */
const FRESH_MS = 20_000;
/** Typing settles this long before a folder is asked for. */
const SETTLE_MS = 80;
/** "Reading" waits this long, so a quick answer never flashes it. */
const QUIET_MS = 180;

/* ---------------------------------------------------------------- paths as people type them */

interface Parsed {
  /** "~", "/" or a drive ("C:"). */
  root: string;
  names: string[];
  sep: "/" | "\\";
}

/** A path as its root and folder names, `.` and `..` resolved by name as the server does. A relative path is under home. */
function parse(path: string): Parsed {
  const p = path.trim();
  let root = "~";
  let rest = p;
  let sep: "/" | "\\" = "/";
  if (p === "~") rest = "";
  else if (/^~[\\/]/.test(p)) {
    rest = p.slice(2);
    if (p[1] === "\\") sep = "\\";
  } else if (/^[A-Za-z]:/.test(p)) {
    root = p.slice(0, 2);
    rest = p.slice(2);
    sep = "\\";
  } else if (p[0] === "/" || p[0] === "\\") root = "/";
  const names: string[] = [];
  for (const n of rest.split(/[\\/]+/)) {
    if (n === "" || n === ".") continue;
    if (n === ".." && names.length > 0 && names[names.length - 1] !== "..") names.pop();
    // Above `/` or a drive is the same place; above `~` only the machine knows.
    else if (n !== ".." || root === "~") names.push(n);
  }
  return { root, names, sep };
}

function format({ root, names, sep }: Parsed): string {
  if (root === "~") return names.length ? `~${sep}${names.join(sep)}` : "~";
  if (root === "/") return `/${names.join("/")}`;
  return `${root}\\${names.join("\\")}`;
}

const clean = (p: string) => format(parse(p));
const endsInSep = (p: string) => /[\\/]$/.test(p);
const join = (dir: string, name: string) => (endsInSep(dir) ? dir + name : `${dir}${parse(dir).sep}${name}`);
/** A folder as the field shows it while it's browsed: ending in a separator, so its folders list. */
const browseText = (p: string) => {
  const c = clean(p);
  return endsInSep(c) ? c : c + parse(c).sep;
};

interface Seg {
  text: string;
  path: string;
}

/** The line's segments: the root (`~`, `/`, `C:`), then each folder, each with the path it stands for. */
function segmentsOf(path: string): Seg[] {
  const p = parse(path);
  return [{ text: p.root, path: format({ ...p, names: [] }) }, ...p.names.map((n, i) => ({ text: n, path: format({ ...p, names: p.names.slice(0, i + 1) }) }))];
}

/** What's typed as the folder it's in and the start of a name in it. */
function split(draft: string): { dir: string; prefix: string } {
  const t = draft.trimStart();
  if (t === "" || t === "~") return { dir: "~/", prefix: "" };
  const i = Math.max(t.lastIndexOf("/"), t.lastIndexOf("\\"));
  return i < 0 ? { dir: "~/", prefix: t } : { dir: t.slice(0, i + 1), prefix: t.slice(i + 1) };
}

/** What to ask the machine for: the folder, or `<folder>/.` while a hidden name is typed (the server then lists hidden folders too). */
const askFor = (dir: string, prefix: string) => (prefix.startsWith(".") ? join(clean(dir), ".") : clean(dir));

/** Folders whose names start with what's typed: same case first, then any case; each kept in recency order. */
function matching(dirs: DirEntry[], prefix: string): DirEntry[] {
  if (!prefix) return dirs;
  const lower = prefix.toLowerCase();
  const exact = dirs.filter((d) => d.name.startsWith(prefix));
  return [...exact, ...dirs.filter((d) => !d.name.startsWith(prefix) && d.name.toLowerCase().startsWith(lower))];
}

const machineOf = (target: string) => (target === "local" ? "the kiln server" : target.replace(/^agent:/, ""));

/* ---------------------------------------------------------------- asking machines */

type Got = { listing: DirListing } | { status: number; message: string };

/** Answers by machine and folder, shared by every picker on the page. */
const answers = new Map<string, { at: number; got: Got }>();

function remembered(key: string | null): Got | null {
  const a = key === null ? undefined : answers.get(key);
  return a && Date.now() - a.at < FRESH_MS ? a.got : null;
}

function failure(e: unknown): Got {
  if (e instanceof ApiError) return { status: e.status, message: e.message };
  return { status: 0, message: e instanceof Error ? e.message : String(e) };
}

type Answer = { key: string; got: Got };

/**
 * One picker's asking, as a store it reads: a folder is asked for while the picker shows it (once
 * typing settles; not at all when a fresh answer is remembered), and the last answer it got stays.
 */
function asker() {
  let answer: Answer | null = null;
  let tell: (() => void) | null = null;
  const give = (a: Answer) => {
    answer = a;
    tell?.();
  };
  return {
    answer: () => answer,
    /** Asks for `path` on `target` for as long as `fn` listens. */
    ask(target: string, path: string, fn: () => void) {
      const key = `${target}\n${path}`;
      tell = fn;
      const known = remembered(key);
      if (known) {
        give({ key, got: known });
        return () => {
          tell = null;
        };
      }
      const ctl = new AbortController();
      const timer = window.setTimeout(() => {
        void machineDirs(target, path, ctl.signal)
          .then((listing): Got => ({ listing }), failure)
          .then((got) => {
            if (ctl.signal.aborted) return;
            // Not reaching the server says nothing about the folder: ask again next time.
            if (!("status" in got && got.status === 0)) answers.set(key, { at: Date.now(), got });
            give({ key, got });
          });
      }, SETTLE_MS);
      return () => {
        window.clearTimeout(timer);
        ctl.abort();
        tell = null;
      };
    },
  };
}

/** The listing of `path` on `target`; while the next one comes, the last answer stays around (`last`). */
function useListing(target: string, path: string) {
  const key = `${target}\n${path}`;
  const [asking] = React.useState(asker);
  const subscribe = React.useCallback((fn: () => void) => asking.ask(target, path, fn), [asking, target, path]);
  const answer = React.useSyncExternalStore(subscribe, asking.answer, asking.answer);
  const hit = remembered(key);
  const fresh = answer?.key === key ? answer.got : hit;
  return { fresh, last: fresh ?? answer?.got ?? null };
}

/** Its words once it has shown for `ms`: what's said while waiting, so a quick answer never flashes it. */
function Quiet({ ms, children }: { ms: number; children: React.ReactNode }) {
  const [since] = React.useState(Date.now);
  return useClock(ms) - since >= ms ? <>{children}</> : null;
}

/** Arrow keys move through a row of words; Up from the first goes back to the field; typing goes on in the field. */
function rove(e: React.KeyboardEvent<HTMLElement>, toField: () => void) {
  const opts = Array.from(e.currentTarget.querySelectorAll<HTMLElement>('[role="option"]'));
  const i = opts.findIndex((o) => o === document.activeElement);
  if (i < 0) return;
  let j: number;
  switch (e.key) {
    case "ArrowRight":
    case "ArrowDown":
      j = Math.min(i + 1, opts.length - 1);
      break;
    case "ArrowLeft":
    case "ArrowUp":
      if (i === 0) {
        e.preventDefault();
        toField();
        return;
      }
      j = i - 1;
      break;
    case "Home":
      j = 0;
      break;
    case "End":
      j = opts.length - 1;
      break;
    default:
      if ((e.key.length === 1 && e.key !== " ") || e.key === "Backspace") {
        if (!e.metaKey && !e.ctrlKey && !e.altKey) toField();
      }
      return;
  }
  e.preventDefault();
  opts[j]?.focus();
}

/* ---------------------------------------------------------------- the picker */

/** A folder on a machine, as a line of its folders that opens in place to browse, type or pick one used before. */
export function InlinePath({ target, value, onChange, recent = [], label = "IN", className }: InlinePathProps) {
  const [open, setOpen] = React.useState(false);
  const [draft, setDraft] = React.useState("");
  /** What it was when it opened: Esc puts it back. */
  const [opened, setOpened] = React.useState(value);
  /** The folder whose words all show (past the first FIRST). */
  const [allIn, setAllIn] = React.useState<string | null>(null);
  const [focused, setFocused] = React.useState<string | null>(null);
  const [width, setWidth] = React.useState(0);
  const [fonts, setFonts] = React.useState(false);
  // Another machine: start over there.
  const [targetWas, setTargetWas] = React.useState(target);
  if (targetWas !== target) {
    setTargetWas(target);
    setOpen(false);
  }
  const root = React.useRef<HTMLDivElement | null>(null);
  const line = React.useRef<HTMLButtonElement>(null);
  const field = React.useRef<HTMLTextAreaElement | null>(null);
  const list = React.useRef<HTMLDivElement>(null);
  /** How the line was last pressed: a tap doesn't bring up the keyboard. */
  const pointer = React.useRef("");
  /** A folder picked from the keyboard: focus goes on to its folders once they're listed. */
  const refocus = React.useRef<"list" | null>(null);
  const panelId = React.useId();
  const recentId = React.useId();
  const machine = machineOf(target);

  const { dir, prefix } = split(draft);
  // Folded, it checks the folder applied (and has it ready for opening); open, the folder browsed.
  const { fresh, last } = useListing(target, open ? askFor(dir, prefix) : clean(value));
  const stale = fresh === null;
  const got = open ? last : fresh;
  const listing = got && "listing" in got ? got.listing : null;
  const problem = got && "status" in got ? got : null;

  const words = listing?.exists && !listing.error ? matching(listing.dirs, prefix) : [];
  const browsing = listing && !stale ? listing.path : clean(dir);
  const shown = prefix || allIn === browsing ? words : words.slice(0, width > 0 && width < 480 ? FIRST_NARROW : FIRST);
  const more = words.length - shown.length;

  // The line: the applied folder; open, the folder browsed carries the underline.
  const segs = segmentsOf(value);
  const here = listing && !stale ? [clean(listing.path), clean(listing.abs)] : [clean(dir)];
  const current = open ? segs.findIndex((s) => here.includes(s.path)) : segs.length - 1;
  const onPath = current >= 0 ? segs[current + 1]?.text : undefined;

  // Too long for one line: the folders after the root give way to "…", one more each pass (before
  // anything is painted) until the rest fits. It starts over when the path or the width changes.
  const fitKey = `${value}\n${label}\n${width}\n${fonts ? 1 : 0}`;
  const [fit, setFit] = React.useState({ key: "", skip: 0 });
  const skip = fit.key === fitKey ? fit.skip : 0;
  const maxSkip = Math.max(0, segs.length - 2);
  const fitPath = React.useCallback(
    (box: HTMLSpanElement | null) => {
      if (!box || skip >= maxSkip) return;
      const end = box.lastElementChild;
      const over = box.scrollWidth > box.clientWidth + 1 || (end instanceof HTMLElement && end.scrollWidth > end.clientWidth + 1);
      if (over) setFit({ key: fitKey, skip: skip + 1 });
    },
    [fitKey, skip, maxSkip],
  );
  // Its width (how many folders show, what fits on the line), and its fonts once they're in.
  const watchRoot = React.useCallback((el: HTMLDivElement | null) => {
    root.current = el;
    if (!el) return;
    const watch = new ResizeObserver((es) => setWidth(Math.round(es[0]?.contentRect.width ?? 0)));
    watch.observe(el);
    void document.fonts?.ready.then(() => setFonts(true));
    return () => watch.disconnect();
  }, []);

  // The field grows to fit what's typed instead of scrolling sideways (again as that, the width or
  // the fonts change).
  const fitField = React.useCallback(
    (f: HTMLTextAreaElement | null) => {
      field.current = f;
      void [draft, width, fonts];
      if (!f) return;
      f.style.height = "0px";
      f.style.height = `${f.scrollHeight}px`;
    },
    [draft, width, fonts],
  );

  // A folder picked from the keyboard: once its folders are listed (looked at whenever the listing
  // shown changes), focus goes to the first of them, else to the field.
  const listed = React.useCallback(
    (panel: HTMLDivElement | null) => {
      void got;
      if (!panel || refocus.current !== "list" || stale) return;
      refocus.current = null;
      const opts = list.current?.querySelectorAll<HTMLElement>('[role="option"]');
      const first = opts && (Array.from(opts).find((o) => !o.hasAttribute("data-up")) ?? opts[0]);
      if (first) first.focus();
      else field.current?.focus();
    },
    [got, stale],
  );

  const browse = (path: string, focus: "list" | null = null) => {
    setDraft(browseText(path));
    setFocused(null);
    if (focus) refocus.current = focus;
  };
  const choose = (path: string, focus: "list" | null) => {
    if (clean(path) !== clean(value)) onChange(path);
    browse(path, focus);
  };
  const fold = (back: boolean) => {
    // What has focus folds away: it goes back to the line.
    if (back && root.current?.contains(document.activeElement)) {
      refocus.current = null;
      line.current?.focus({ preventScroll: true });
    }
    setOpen(false);
    setAllIn(null);
  };
  const apply = () => {
    // A folder ending in a separator is the one listed: take the machine's own spelling of it.
    const typed = prefix === "" && listing && !stale ? listing.path : clean(draft);
    if (clean(typed) !== clean(value)) onChange(typed);
    fold(true);
  };
  const revert = () => {
    if (clean(opened) !== clean(value)) onChange(opened);
    fold(true);
  };
  const toField = () => {
    const f = field.current;
    f?.focus();
    f?.setSelectionRange(f.value.length, f.value.length);
  };
  const first = !stale && prefix ? words[0] : undefined;
  const complete = () => {
    if (first) browse(join(dir, first.name));
  };

  // Esc puts back what it was. Taken before the sheet around it (a Radix dialog, which listens on
  // the document) would close on it.
  useListener<KeyboardEvent>(
    window,
    "keydown",
    (e) => {
      if (e.key !== "Escape" || e.isComposing || e.defaultPrevented) return;
      if (!root.current?.contains(document.activeElement)) return;
      e.preventDefault();
      revert();
    },
    { on: open, capture: true },
  );
  // Tapping anywhere else folds it, keeping what's applied.
  useListener<PointerEvent>(
    document,
    "pointerdown",
    (e) => {
      if (e.target instanceof Node && root.current?.contains(e.target)) return;
      fold(false);
    },
    { on: open, capture: true },
  );

  const onLine = (e: React.MouseEvent<HTMLButtonElement>) => {
    const seg = e.target instanceof Element ? e.target.closest<HTMLElement>("[data-seg]") : null;
    const at = seg ? Number(seg.dataset.seg) : null;
    const to = at === null ? undefined : segs[at];
    if (!open) {
      // Keys and mice go on typing in the field as it opens; a tap leaves the keyboard down.
      const type = e.detail === 0 || pointer.current === "mouse" || pointer.current === "pen";
      flushSync(() => {
        setOpened(value);
        setAllIn(null);
        setOpen(true);
        browse(to?.path ?? value);
      });
      if (type) {
        refocus.current = null;
        toField();
      }
    } else if (!to || at === current) {
      fold(true);
    } else {
      browse(to.path);
    }
  };

  const pick = (path: string, e: React.MouseEvent) => {
    if (stale) return;
    choose(path, e.detail === 0 ? "list" : null);
  };

  // One line under the field (folded: under the line) for what's going on.
  let tone: "warn" | undefined;
  let say: React.ReactNode = null;
  if (open && stale) {
    // Said once it has taken a moment, so a quick answer never flashes it.
    say = (
      <Quiet ms={QUIET_MS}>
        <span className="ip-optic" aria-hidden="true">
          <Optic state="idle" />
        </span>
        Reading {clean(dir)} on {machine}
      </Quiet>
    );
  } else if (open && !stale && problem) {
    tone = "warn";
    say = problem.status === 409 && /too old/i.test(problem.message) ? `Can't list folders on ${machine} (its runner is too old). Type a path` : `Can't list folders on ${machine}: ${problem.message}`;
  } else if (listing && !stale && !listing.exists) {
    tone = "warn";
    say = `No folder ${listing.path} on ${machine}`;
  } else if (listing && !stale && listing.error) {
    tone = "warn";
    say = `Can't open ${listing.path} on ${machine}: ${listing.error}`;
  } else if (open && listing && !stale) {
    if (prefix && words.length === 0) say = `Nothing in ${listing.path} starts with “${prefix}”`;
    else if (listing.dirs.length === 0) say = `No folders in ${listing.path}`;
    else if (listing.truncated && more === 0) say = `The ${listing.dirs.length} most recently changed; type the name of any other`;
  }

  // The line's words: the root, "…" for the folders that gave way (tapping it browses the last of
  // them), then the rest. A `/` root is its own separator.
  const items: { at: number; text: string; current: boolean; last: boolean }[] = [];
  segs.forEach((s, i) => {
    if (i === 0 || i > skip) items.push({ at: i, text: s.text, current: i === current, last: i === segs.length - 1 });
    else if (i === skip) items.push({ at: i, text: "…", current: current >= 1 && current <= skip, last: false });
  });
  const bare = segs[0]?.text === "/";
  const lineWords = items.flatMap((it, k) => [
    ...(k > 0 && !(k === 1 && bare) ? [<Sep key={`sep-${it.at}`} />] : []),
    <Segment key={it.at} at={it.at} text={it.text} current={it.current} last={it.last} />,
  ]);

  const stop = [focused, onPath, words[0]?.name].find((n) => n != null && shown.some((d) => d.name === n));
  const hasList = open && (listing?.parent != null || shown.length > 0);
  const recentStop = recent.find((r) => clean(r) === clean(value)) ?? recent[0];

  return (
    <div ref={watchRoot} className={cn("ip", className)} data-open={open ? "" : undefined}>
      <button
        ref={line}
        type="button"
        className="ip-line"
        aria-expanded={open}
        aria-controls={panelId}
        aria-label={`${label} ${clean(value)}`}
        onPointerDown={(e) => {
          pointer.current = e.pointerType;
        }}
        onClick={onLine}
      >
        <span className="ip-eyebrow">{label}</span>
        <span ref={fitPath} className="ip-path">
          {lineWords}
        </span>
      </button>
      <div ref={listed} id={panelId} className="ip-panel">
        {open && (
          <textarea
            ref={fitField}
            className="ip-field"
            rows={1}
            value={draft}
            aria-label="Folder path"
            aria-controls={hasList ? `${panelId}-list` : undefined}
            placeholder="~/Projects/repo"
            spellCheck={false}
            autoCapitalize="off"
            autoCorrect="off"
            autoComplete="off"
            enterKeyHint="go"
            onChange={(e) => {
              setDraft(e.target.value.replace(/[\r\n]+/g, ""));
              setFocused(null);
            }}
            onKeyDown={(e) => {
              if (e.nativeEvent.isComposing) return;
              const f = e.currentTarget;
              const atEnd = f.selectionStart === f.value.length && f.selectionEnd === f.value.length;
              if (e.key === "Enter") {
                e.preventDefault();
                apply();
              } else if (first && ((e.key === "Tab" && !e.shiftKey) || (e.key === "ArrowRight" && atEnd))) {
                e.preventDefault();
                complete();
              } else if (e.key === "ArrowDown" && atEnd && !stale) {
                // Into the folders, at the one the line goes through (else the first).
                const to = list.current?.querySelector<HTMLElement>('[role="option"][tabindex="0"]');
                if (to) {
                  e.preventDefault();
                  to.focus();
                }
              }
            }}
          />
        )}
        <p className="ip-status" data-tone={tone} role="status">
          {say}
        </p>
        {hasList && (
          <div className="ip-words" data-stale={stale ? "" : undefined}>
            <div
              ref={list}
              id={`${panelId}-list`}
              role="listbox"
              aria-label={`Folders in ${browsing}`}
              aria-busy={stale}
              className="ip-list"
              onKeyDown={(e) => rove(e, toField)}
              onBlur={(e) => {
                if (!e.currentTarget.contains(e.relatedTarget as Node | null)) setFocused(null);
              }}
            >
              {listing?.parent != null && (
                <button
                  type="button"
                  role="option"
                  aria-selected={false}
                  aria-label={`Up to ${listing.parent}`}
                  data-up=""
                  tabIndex={stop === undefined ? 0 : -1}
                  className="ip-word ip-up"
                  onClick={(e) => pick(listing.parent ?? "~", e)}
                >
                  <span className="ip-word-text">↑ ..</span>
                </button>
              )}
              {shown.map((d) => (
                <button
                  key={d.name}
                  type="button"
                  role="option"
                  aria-selected={d.name === onPath}
                  tabIndex={d.name === stop ? 0 : -1}
                  title={d.modified ? `Changed ${relativeTime(d.modified)}` : undefined}
                  className="ip-word"
                  onFocus={() => setFocused(d.name)}
                  onClick={(e) => listing && pick(join(listing.path, d.name), e)}
                >
                  <span className="ip-word-text">{d.name}</span>
                </button>
              ))}
            </div>
            {more > 0 && (
              <button type="button" className="ip-word ip-more" aria-label={`Show ${more} more folders`} onClick={() => setAllIn(browsing)}>
                <span className="ip-word-text">+{more} more</span>
              </button>
            )}
          </div>
        )}
        {open && recent.length > 0 && (
          <div className="ip-row">
            <span id={recentId} className="ip-eyebrow ip-key">
              Recent
            </span>
            <div role="listbox" aria-labelledby={recentId} className="ip-list" onKeyDown={(e) => rove(e, toField)}>
              {recent.map((r) => (
                <button
                  key={r}
                  type="button"
                  role="option"
                  aria-selected={clean(r) === clean(value)}
                  tabIndex={r === recentStop ? 0 : -1}
                  className="ip-word"
                  onClick={(e) => choose(r, e.detail === 0 ? "list" : null)}
                >
                  <span className="ip-word-text">{r}</span>
                </button>
              ))}
            </div>
          </div>
        )}
      </div>
    </div>
  );
}

function Sep() {
  return (
    <span className="ip-sep" aria-hidden="true">
      /
    </span>
  );
}

function Segment({ at, text, current, last }: { at: number; text: string; current: boolean; last?: boolean }) {
  return (
    <span className="ip-seg" data-seg={at} data-current={current ? "" : undefined} data-last={last ? "" : undefined}>
      {text}
    </span>
  );
}
