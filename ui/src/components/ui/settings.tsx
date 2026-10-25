/**
 * The settings page's parts. Settings come in sections that open and close; a section's header says
 * what's in it, and the page remembers which are open. In a section, groups of related settings,
 * each with a title and what it's for; in a group, rows: a setting's name and what it does on the
 * left, its control on the right (under them on a phone). Search narrows the page to the groups whose
 * words match, and opens their sections. One type scale throughout (settings.css).
 *
 * A group is found by its title, what it says it's for, and its `words` (what's in it: the names of
 * its fields and controls). A section's title or `words` matching shows every group in it.
 */
import * as React from "react";
import { ChevronDown, Search, X } from "lucide-react";
import { getPref, setPref, usePref } from "@/lib/prefs";
import { cn } from "@/lib/utils";
import "./settings.css";

/** Which sections and groups are open: `{ [id]: true }`. */
const OPEN = "kiln.settings.open";
const REVEAL = "kiln:settings-open";

/** The words searched for, lower case; none while not searching. */
const QueryContext = React.createContext<string[]>([]);
/** The section's own words matched the search: every group in it shows. */
const WholeContext = React.createContext(false);

/** Search opens matches temporarily; toggling them never changes the saved layout. */
function useDisclosure(id: string, words: string[]) {
  const [open, setOpen] = usePref<Record<string, boolean>>(OPEN, {});
  const [searched, setSearched] = React.useState<{ words: string[]; open: boolean } | null>(null);
  const searching = words.length > 0;
  const shown = searching ? (searched?.words === words ? searched.open : true) : open[id] === true;
  const toggle = () => {
    if (searching) setSearched({ words, open: !shown });
    else setOpen({ ...getPref<Record<string, boolean>>(OPEN, {}), [id]: !shown });
  };
  return [shown, toggle] as const;
}

/** A query's words, lower case. */
export const wordsOf = (q: string) => q.toLowerCase().split(/\s+/).filter(Boolean);

/** Every word is somewhere in the texts. */
export function matches(words: string[], ...texts: (string | undefined | null)[]): boolean {
  const hay = texts.filter(Boolean).join(" ").toLowerCase();
  return words.every((w) => hay.includes(w));
}

/** Open sections and groups from outside them: a link to one, "Change how long kiln waits". */
export function openSettings(...ids: string[]) {
  const open = getPref<Record<string, boolean>>(OPEN, {});
  setPref(OPEN, {
    ...open,
    ...Object.fromEntries(ids.map((id) => [id, true])),
  });
  // A direct link takes precedence over a filter that could hide its destination.
  if (typeof window !== "undefined") window.dispatchEvent(new Event(REVEAL));
}

/** The section or group a link asked for, until it's in view (or you scroll on your own). */
let sought: string | null = null;

/**
 * Open what `#hash` names, a section (`#deployments`) or a group (`#deployments.accounts`), and bring it into
 * view as it's drawn: again when what it waited on loads (its place moves as what's above it
 * fills in), until you scroll yourself. For the page's root: called once, as it mounts.
 */
export function openHash(hash: string) {
  let id: string;
  try {
    id = decodeURIComponent(hash.replace(/^#/, ""));
  } catch {
    return;
  }
  if (!id) return;
  const section = id.split(".")[0] ?? id;
  sought = id;
  const stop = () => {
    sought = null;
    for (const ev of ["wheel", "touchstart", "keydown"] as const) window.removeEventListener(ev, stop);
  };
  for (const ev of ["wheel", "touchstart", "keydown"] as const) window.addEventListener(ev, stop, { passive: true });
  window.setTimeout(stop, 6000);
  openSettings(...new Set([section, id]));
  // Already drawn (a section is, closed): into view now; what opens in it brings it again.
  requestAnimationFrame(() => seek(document.getElementById(id)));
}

/** For a section's or group's element: scrolled into view if a link asked for it. */
function seek(el: HTMLElement | null) {
  if (!el || el.id !== sought) return;
  requestAnimationFrame(() => {
    if (el.isConnected && el.id === sought) el.scrollIntoView({ block: "start" });
  });
}

export function SettingsSearch({ value, onChange }: { value: string; onChange: (v: string) => void }) {
  const input = React.useRef<HTMLInputElement>(null);
  return (
    <div className="field st-search">
      <Search size={14} aria-hidden="true" />
      <input
        ref={input}
        type="search"
        value={value}
        placeholder="Search settings"
        aria-label="Search settings"
        aria-controls="settings-sections"
        enterKeyHint="search"
        onChange={(e) => onChange(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Escape" && value) {
            e.preventDefault();
            onChange("");
          }
        }}
      />
      {value && (
        <button type="button" className="st-search-clear" aria-label="Clear the search" onClick={() => { onChange(""); input.current?.focus(); }}>
          <X size={14} />
        </button>
      )}
    </div>
  );
}

/** The sections, narrowed to what `query` finds. */
export function SettingsSections({ query, onQueryChange, children }: { query: string; onQueryChange?: (value: string) => void; children: React.ReactNode }) {
  const words = React.useMemo(() => wordsOf(query), [query]);
  const connect = React.useCallback((el: HTMLDivElement | null) => {
    if (!el || !onQueryChange) return;
    const reveal = () => onQueryChange("");
    window.addEventListener(REVEAL, reveal);
    return () => window.removeEventListener(REVEAL, reveal);
  }, [onQueryChange]);
  return (
    <QueryContext.Provider value={words}>
      <div ref={connect} id="settings-sections" className="st-sections" data-searching={words.length > 0 || undefined}>
        {children}
        {/* Shown (settings.css) only while searching and no group shows. */}
        <p className="st-none" role="status">Nothing in Settings matches &ldquo;{query.trim()}&rdquo;.</p>
      </div>
    </QueryContext.Provider>
  );
}

/**
 * A section that opens and closes: its number, title and what's in it, and on the right what it
 * says now (`status`, which may load). Its groups are drawn only while it's open (or searched).
 */
export function SettingsSection({
  id,
  index,
  title,
  about,
  words: more,
  status,
  children,
}: {
  id: string;
  index: number;
  title: string;
  about: string;
  /** More words that find it. */
  words?: string;
  status?: React.ReactNode;
  children: React.ReactNode;
}) {
  const words = React.useContext(QueryContext);
  const searching = words.length > 0;
  const [shown, toggle] = useDisclosure(id, words);
  // Its title or words, not what it says it holds: a word of one group's finds that group alone.
  const whole = searching && matches(words, title, more);
  return (
    <section ref={seek} id={id} className="st-section" data-open={shown || undefined} aria-labelledby={`${id}-t`}>
      <h2 className="st-section-h">
        <button
          type="button"
          className="st-section-head"
          aria-expanded={shown}
          aria-controls={`${id}-b`}
          onClick={toggle}
        >
          <span className="st-index" aria-hidden="true">
            {String(index).padStart(2, "0")}
          </span>
          <span className="st-section-words">
            <span id={`${id}-t`} className="st-section-title">
              {title}
            </span>
            <span className="st-section-about">{about}</span>
          </span>
          {status && (
            <span className="st-section-status">
              <React.Suspense fallback={null}>{status}</React.Suspense>
            </span>
          )}
          <ChevronDown className="st-chev" aria-hidden="true" />
        </button>
      </h2>
      {(shown || searching) && (
        // Keep matching groups mounted while a search result is collapsed: CSS uses their
        // presence to determine whether this section belongs in the search results.
        <div id={`${id}-b`} className="st-section-body" hidden={!shown}>
          <WholeContext.Provider value={whole}>{children}</WholeContext.Provider>
        </div>
      )}
    </section>
  );
}

/**
 * Related settings under one title: what they're for, actions on the right, then their rows. One
 * that's `collapsible` (settings seldom changed) opens on a tap, and stays as it was left.
 */
export function SettingsGroup({
  id,
  title,
  about,
  words: more,
  actions,
  collapsible,
  className,
  children,
}: {
  id: string;
  title: string;
  /** What it's for: a sentence, or what it says now. */
  about?: React.ReactNode;
  /** More words that find it: the names of its fields and controls. */
  words?: string;
  actions?: React.ReactNode;
  collapsible?: boolean;
  className?: string;
  children?: React.ReactNode;
}) {
  const words = React.useContext(QueryContext);
  const whole = React.useContext(WholeContext);
  const searching = words.length > 0;
  const [expanded, toggle] = useDisclosure(id, words);
  if (searching && !whole && !matches(words, title, typeof about === "string" ? about : undefined, more)) return null;
  const shown = !collapsible || expanded;
  const head = (
    <>
      <h3 className="st-group-title">{title}</h3>
      {about && <p className="st-group-about">{about}</p>}
    </>
  );
  return (
    <div ref={seek} id={id} className={cn("st-group", className)} data-open={shown || undefined}>
      <div className="st-group-head">
        {collapsible ? (
          <button
            type="button"
            className="st-group-toggle"
            aria-expanded={shown}
            aria-controls={`${id}-b`}
            onClick={toggle}
          >
            <span className="min-w-0">{head}</span>
            <ChevronDown className="st-chev" aria-hidden="true" />
          </button>
        ) : (
          <div className="min-w-0">{head}</div>
        )}
        {actions && <div className="st-group-actions">{actions}</div>}
      </div>
      {shown && children && <div id={`${id}-b`} className="st-group-body">{children}</div>}
    </div>
  );
}

/**
 * One setting: its name and what it does, and its control. On a phone the control goes under the
 * words, unless it's `inline` (a checkbox, a small button).
 */
export function SettingRow({
  label,
  help,
  htmlFor,
  inline,
  className,
  children,
}: {
  label: React.ReactNode;
  help?: React.ReactNode;
  /** The control's id, when it's one field: its name then labels it. */
  htmlFor?: string;
  inline?: boolean;
  className?: string;
  children?: React.ReactNode;
}) {
  return (
    <div className={cn("st-row", className)} data-inline={inline || undefined}>
      <div className="st-row-words">
        {htmlFor ? (
          <label htmlFor={htmlFor} className="st-label">
            {label}
          </label>
        ) : (
          <span className="st-label">{label}</span>
        )}
        {help && <p className="st-help">{help}</p>}
      </div>
      {children && <div className="st-row-control">{children}</div>}
    </div>
  );
}
