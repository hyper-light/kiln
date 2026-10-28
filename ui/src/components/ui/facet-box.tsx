/**
 * A filter field: its list offers every facet (runner, harness, state…) as checkboxes, each with how
 * many it would show, and what you type finds them (and, given `onQuery`, matches names in the
 * caller's list too). What's chosen sits in the field as chips you can remove. The field is one line
 * at one height (--field-h) whatever is chosen: chips that don't fit fold into "+N" after the last
 * one that does (facet-fold.ts). Open (a click in it, ↓, typing, or +N), it lifts over the page with
 * every chip, its list under it, like a select's: the box it leaves keeps its place, so nothing moves.
 * In a FilterBar on a phone it spans the bar. Within a facet, choices widen (any of them); across
 * facets they narrow (all of them).
 */
import * as React from "react";
import { Search, X } from "lucide-react";
import { cn } from "@/lib/utils";
import { foldChips, sameFold, typingRoom, UNFOLDED, type Fold } from "./facet-fold";

export interface FacetOption {
  id: string;
  label: string;
  /** How many would show with it chosen (the other facets as they are), when known. */
  count?: number;
  /** More about it, on hover (a folder's whole path). */
  title?: string;
}

export interface Facet {
  id: string;
  label: string;
  options: FacetOption[];
}

export type Chosen = Record<string, string[]>;

/**
 * The fold the line has room for, read from its measuring copy (facet-measure): every chip at its own
 * width, then the "+N" chip at its widest, the chip that counts them all, and the words typed.
 */
function measure(line: HTMLElement, copy: HTMLElement, typing: boolean): Fold {
  const widths = Array.from(copy.children, (el) => el.getBoundingClientRect().width);
  const words = widths.pop() ?? 0;
  const count = widths.pop() ?? 0;
  const more = widths.pop() ?? 0;
  const width = line.clientWidth;
  const gap = parseFloat(getComputedStyle(line).columnGap) || 0;
  return foldChips({ line: width, chips: widths, more, count, typed: typingRoom(width, words, typing), gap });
}

export function FacetBox({
  label,
  placeholder,
  query: queryProp,
  onQuery: onQueryProp,
  facets,
  chosen,
  onChosen,
  className,
}: {
  /** What the field filters ("Filter agents"). */
  label: string;
  placeholder: string;
  /** The words typed, when they also match names in the caller's list (kept by the caller). */
  query?: string;
  onQuery?: (q: string) => void;
  facets: Facet[];
  chosen: Chosen;
  onChosen: (next: Chosen) => void;
  className?: string;
}) {
  const [open, setOpen] = React.useState(false);
  const [active, setActive] = React.useState(-1);
  const [fold, setFold] = React.useState<Fold>(UNFOLDED);
  const input = React.useRef<HTMLInputElement>(null);
  const root = React.useRef<HTMLDivElement>(null);
  const uid = React.useId();
  const listId = `${uid}-list`;
  // Without the caller's words, the field keeps its own: they only find options.
  const [own, setOwn] = React.useState("");
  const query = queryProp ?? own;
  const onQuery = onQueryProp ?? setOwn;

  // The typed words also find options (a facet named by them shows all of its own).
  const q = query.trim().toLowerCase();
  const groups = facets
    .map((f) => ({
      ...f,
      options: !q || f.label.toLowerCase().includes(q) ? f.options : f.options.filter((o) => o.label.toLowerCase().includes(q)),
    }))
    .filter((f) => f.options.length > 0);
  const flat = groups.flatMap((f) => f.options.map((o) => ({ facet: f, o })));
  const optionId = (i: number) => `${uid}-o${i}`;

  const isOn = (facet: string, id: string) => (chosen[facet] ?? []).includes(id);
  const toggle = (facet: string, id: string) => {
    const now = chosen[facet] ?? [];
    const next = {
      ...chosen,
      [facet]: now.includes(id) ? now.filter((x) => x !== id) : [...now, id],
    };
    if (next[facet]?.length === 0) delete next[facet];
    onChosen(next);
  };
  const chips = facets.flatMap((f) =>
    (chosen[f.id] ?? []).map((id) => ({
      facet: f,
      id,
      label: f.options.find((o) => o.id === id)?.label ?? id,
    })),
  );
  const any = chips.length > 0 || query.length > 0;

  // Closed, the line shows the chips that fit and folds the rest into +N; open, it shows them all.
  const shown = open ? chips.length : Math.min(fold.shown, chips.length);
  const folded = chips.slice(shown);
  const cut = !open && shown > 0 ? fold.cut : undefined;
  const moreLabel = shown === 0 && fold.count === "words" ? `${chips.length} filters` : `+${folded.length}`;
  // A new ref when the chips, the words or open change: React hands it the line once they're on the
  // page, and it measures there, then again whenever the line or a chip changes size (the window, a
  // font arriving).
  const chipKey = chips.map((c) => `${c.facet.id}:${c.id}:${c.label}`).join("\n");
  const fit = React.useCallback(
    (line: HTMLDivElement | null) => {
      const copy = line?.querySelector<HTMLElement>(".facet-measure");
      if (!line || !copy) return;
      const update = () => {
        const next = open ? UNFOLDED : measure(line, copy, query !== "");
        setFold((was) => (sameFold(was, next) ? was : next));
      };
      // Opened with more chips than fit, the words typed are at the end of them: in view.
      if (open) line.scrollTop = line.scrollHeight;
      update();
      const watch = new ResizeObserver(update);
      watch.observe(line);
      for (const el of copy.children) watch.observe(el);
      return () => watch.disconnect();
    },
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [chipKey, query, open],
  );

  // Closes when you click or tab anywhere else (listened for only while its list is open).
  const away = React.useCallback((pop: HTMLDivElement | null) => {
    if (!pop) return;
    const onDown = (e: PointerEvent) => {
      if (!root.current?.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener("pointerdown", onDown);
    return () => document.removeEventListener("pointerdown", onDown);
  }, []);
  /** Move the highlight, keeping it in view. */
  const highlight = (i: number) => {
    setActive(i);
    if (i >= 0) document.getElementById(optionId(i))?.scrollIntoView({ block: "nearest" });
  };

  const pick = (i: number) => {
    const it = flat[i];
    if (!it) return;
    toggle(it.facet.id, it.o.id);
    // The words were typed to find it: they go, so the names aren't filtered by them too.
    if (q) {
      onQuery("");
      setActive(-1);
    }
  };

  const onKeyDown = (e: React.KeyboardEvent<HTMLInputElement>) => {
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      if (!open) {
        setOpen(true);
        setActive(e.key === "ArrowDown" ? 0 : flat.length - 1);
        return;
      }
      if (flat.length === 0) return;
      highlight(e.key === "ArrowDown" ? (active + 1) % flat.length : active <= 0 ? flat.length - 1 : active - 1);
    } else if (e.key === "Enter") {
      if (open && active >= 0) {
        e.preventDefault();
        pick(active);
      } else if (open) {
        e.preventDefault();
        setOpen(false);
      }
    } else if (e.key === "Escape") {
      if (open) {
        e.preventDefault();
        setOpen(false);
      } else if (query) {
        e.preventDefault();
        onQuery("");
      }
    } else if (e.key === "Backspace" && query === "" && chips.length > 0) {
      // The last chip folded into +N isn't in view: the field opens first, so you see it go.
      if (!open && folded.length > 0) {
        e.preventDefault();
        setOpen(true);
        return;
      }
      const last = chips[chips.length - 1];
      if (last) toggle(last.facet.id, last.id);
    } else if (e.key === "Tab") {
      setOpen(false);
    }
  };

  let index = -1;
  return (
    <div ref={root} className={cn("facet-box", className)} data-open={open || undefined}>
      <div className="facet-layer">
        {/* A click anywhere in the field is a click in its text. */}
        <div
          className="field"
          onPointerDown={(e) => {
            if ((e.target as Element).closest("button, input, .facet-chip")) return;
            e.preventDefault();
            input.current?.focus();
            setOpen(true);
          }}
        >
          <Search size={15} aria-hidden="true" />
          <div ref={fit} className="facet-line">
            {chips.slice(0, shown).map((c, i) => (
              <span key={`${c.facet.id}:${c.id}`} className="facet-chip" title={`${c.facet.label}: ${c.label}`} style={i === 0 && cut !== undefined ? { maxWidth: cut } : undefined}>
                <span>{c.label}</span>
                <button type="button" aria-label={`Remove ${c.facet.label}: ${c.label}`} onClick={() => toggle(c.facet.id, c.id)}>
                  <X size={12} />
                </button>
              </span>
            ))}
            {folded.length > 0 && (
              <button
                type="button"
                className="facet-more"
                aria-label={`Show ${folded.length} more: ${folded.map((c) => c.label).join(", ")}`}
                aria-expanded={open}
                aria-controls={listId}
                title={folded.map((c) => `${c.facet.label}: ${c.label}`).join("\n")}
                onPointerDown={(e) => e.preventDefault()}
                onClick={() => {
                  input.current?.focus();
                  setOpen(true);
                }}
              >
                {moreLabel}
              </button>
            )}
            <input
              ref={input}
              type="text"
              role="combobox"
              aria-label={label}
              aria-expanded={open}
              aria-controls={listId}
              aria-autocomplete="list"
              aria-activedescendant={open && active >= 0 ? optionId(active) : undefined}
              value={query}
              placeholder={chips.length ? "" : placeholder}
              autoComplete="off"
              spellCheck={false}
              onChange={(e) => {
                onQuery(e.target.value);
                setOpen(true);
                setActive(-1);
              }}
              onClick={() => setOpen(true)}
              onKeyDown={onKeyDown}
            />
            {/* Unseen: every chip at its own width, what the fold is measured from. */}
            <div className="facet-measure" aria-hidden="true">
              {chips.map((c) => (
                <span key={`${c.facet.id}:${c.id}`} className="facet-chip">
                  <span>{c.label}</span>
                  <span className="facet-chip-x" />
                </span>
              ))}
              <span className="facet-more">+{chips.length}</span>
              <span className="facet-more">{chips.length} filters</span>
              <span className="facet-typed">{query}</span>
            </div>
          </div>
          {any && (
            <button
              type="button"
              className="field-clear"
              aria-label="Clear the filter"
              onClick={() => {
                onQuery("");
                onChosen({});
                input.current?.focus();
              }}
            >
              <X size={14} />
            </button>
          )}
        </div>
        {open && (
          <div ref={away} className="facet-pop popover">
            <div id={listId} role="listbox" aria-multiselectable="true" aria-label={label}>
              {groups.length === 0 && <p className="facet-none">{onQueryProp ? `Filtering names by “${query.trim()}”.` : `Nothing matches “${query.trim()}”.`}</p>}
              {groups.map((f) => (
                <div key={f.id} role="group" aria-labelledby={`${uid}-${f.id}`} className="facet-group">
                  <div id={`${uid}-${f.id}`} className="facet-label">
                    {f.label}
                  </div>
                  {f.options.map((o) => {
                    index += 1;
                    const i = index;
                    const on = isOn(f.id, o.id);
                    return (
                      <div
                        key={o.id}
                        id={optionId(i)}
                        role="option"
                        aria-selected={on}
                        className="facet-opt"
                        data-active={i === active || undefined}
                        data-empty={(!on && o.count === 0) || undefined}
                        title={o.title}
                        onPointerDown={(e) => e.preventDefault()}
                        onPointerEnter={() => setActive(i)}
                        onClick={() => pick(i)}
                      >
                        <span className="facet-mark" aria-hidden="true" />
                        <span className="facet-name">{o.label}</span>
                        {o.count !== undefined && <span className="facet-count">{o.count}</span>}
                      </div>
                    );
                  })}
                </div>
              ))}
            </div>
            <div className="facet-foot">
              <button
                type="button"
                className="facet-act"
                disabled={!any}
                onPointerDown={(e) => e.preventDefault()}
                onClick={() => {
                  onQuery("");
                  onChosen({});
                }}
              >
                Clear
              </button>
              <button type="button" className="facet-act" onPointerDown={(e) => e.preventDefault()} onClick={() => setOpen(false)}>
                Done
              </button>
            </div>
          </div>
        )}
      </div>
    </div>
  );
}
