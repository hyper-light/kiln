/**
 * A filter field: its list offers every facet (runner, harness, state…) as checkboxes, each with how
 * many it would show, and what you type finds them (and, given `onQuery`, matches names in the
 * caller's list too). What's chosen sits in the field as chips you can remove. The list opens under
 * the field, over the page, like a select's: nothing moves. Within a facet, choices widen (any of
 * them); across facets they narrow (all of them).
 */
import * as React from "react";
import { Search, X } from "lucide-react";
import { cn } from "@/lib/utils";

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
      const last = chips[chips.length - 1];
      if (last) toggle(last.facet.id, last.id);
    } else if (e.key === "Tab") {
      setOpen(false);
    }
  };

  let index = -1;
  return (
    <div ref={root} className={cn("facet-box", className)}>
      {/* A click anywhere in the field is a click in its text. */}
      <div
        className="field"
        onPointerDown={(e) => {
          if (e.target === e.currentTarget) {
            e.preventDefault();
            input.current?.focus();
            setOpen(true);
          }
        }}
      >
        <Search size={15} aria-hidden="true" />
        {chips.map((c) => (
          <span key={`${c.facet.id}:${c.id}`} className="facet-chip" title={`${c.facet.label}: ${c.label}`}>
            <span>{c.label}</span>
            <button type="button" aria-label={`Remove ${c.facet.label}: ${c.label}`} onClick={() => toggle(c.facet.id, c.id)}>
              <X size={12} />
            </button>
          </span>
        ))}
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
        {any && (
          <button
            type="button"
            className="facet-clear"
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
  );
}
