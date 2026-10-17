/**
 * A filterable select box with faceted checkbox groups and counts: one field whose list (like a
 * native select's) holds each facet's values, each with a checkbox and how many it would show.
 * Typing narrows the values; closed, the field says what is chosen. Values within a facet widen
 * the choice (any of them), facets narrow it (all of them). Keyboard: arrows move, Enter or Space
 * ticks, Escape closes.
 */
import * as React from "react";
import { Check, ChevronDown, X } from "lucide-react";
import { cn } from "@/lib/utils";

export interface FacetValue {
  id: string;
  label: string;
  /** How many the value would show, if known. */
  count?: number;
}

export interface FacetGroup {
  id: string;
  label: string;
  values: FacetValue[];
}

/** Chosen values by facet id. */
export type FacetChoice = Record<string, string[]>;

export function FacetSelect({
  label,
  groups,
  chosen,
  onChange,
  placeholder = "Anything",
  className,
}: {
  /** What the field filters, for assistive technology ("Filter apps"). */
  label: string;
  groups: FacetGroup[];
  chosen: FacetChoice;
  onChange: (next: FacetChoice) => void;
  /** What the closed field says when nothing is chosen. */
  placeholder?: string;
  className?: string;
}) {
  const [open, setOpen] = React.useState(false);
  const [text, setText] = React.useState("");
  const [active, setActive] = React.useState(0);
  const root = React.useRef<HTMLDivElement>(null);
  const input = React.useRef<HTMLInputElement>(null);
  const id = React.useId();

  const needle = text.trim().toLowerCase();
  const shown = groups
    .map((g) => ({ ...g, values: g.values.filter((v) => !needle || v.label.toLowerCase().includes(needle) || g.label.toLowerCase().includes(needle)) }))
    .filter((g) => g.values.length > 0);
  const flat = shown.flatMap((g) => g.values.map((v) => ({ group: g.id, value: v.id })));
  const on = (g: string, v: string) => (chosen[g] ?? []).includes(v);
  const toggle = (g: string, v: string) => {
    const now = chosen[g] ?? [];
    onChange({ ...chosen, [g]: now.includes(v) ? now.filter((x) => x !== v) : [...now, v] });
  };
  const any = groups.some((g) => (chosen[g.id] ?? []).length > 0);
  const summary = groups
    .filter((g) => (chosen[g.id] ?? []).length > 0)
    .map((g) => (chosen[g.id] ?? []).map((v) => g.values.find((x) => x.id === v)?.label ?? v).join(" or "))
    .join(" · ");

  const close = () => {
    setOpen(false);
    setText("");
  };

  // A press anywhere else closes it, as a select's list closes.
  React.useEffect(() => {
    if (!open) return;
    const away = (e: PointerEvent) => {
      if (!root.current?.contains(e.target as Node)) {
        setOpen(false);
        setText("");
      }
    };
    document.addEventListener("pointerdown", away);
    return () => document.removeEventListener("pointerdown", away);
  }, [open]);

  const onKey = (e: React.KeyboardEvent<HTMLInputElement>) => {
    if (e.key === "ArrowDown") {
      e.preventDefault();
      if (!open) setOpen(true);
      else setActive((a) => Math.min(a + 1, flat.length - 1));
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      setActive((a) => Math.max(a - 1, 0));
    } else if ((e.key === "Enter" || (e.key === " " && !text)) && open) {
      e.preventDefault();
      const o = flat[active];
      if (o) toggle(o.group, o.value);
    } else if (e.key === "Escape" && open) {
      e.preventDefault();
      close();
    } else if (e.key === "Tab") {
      close();
    }
  };

  return (
    <div ref={root} className={cn("fs", className)} data-open={open || undefined}>
      <div
        className="field fs-field"
        onClick={() => {
          setOpen(true);
          input.current?.focus();
        }}
      >
        <input
          ref={input}
          role="combobox"
          aria-label={label}
          aria-expanded={open}
          aria-controls={`${id}-list`}
          aria-autocomplete="list"
          aria-activedescendant={open && flat[active] ? `${id}-${active}` : undefined}
          value={open ? text : summary}
          placeholder={open ? "Type to narrow" : placeholder}
          readOnly={!open}
          onChange={(e) => {
            setText(e.target.value);
            setActive(0);
          }}
          onFocus={() => setOpen(true)}
          onKeyDown={onKey}
        />
        {any && (
          <button
            type="button"
            className="fs-clear"
            aria-label={`Clear: ${label.toLowerCase()}`}
            onClick={(e) => {
              e.stopPropagation();
              onChange({});
            }}
          >
            <X size={13} aria-hidden="true" />
          </button>
        )}
        <ChevronDown size={14} aria-hidden="true" className="fs-chev" />
      </div>
      {open && (
        <div id={`${id}-list`} role="listbox" aria-multiselectable="true" aria-label={label} className="fs-list">
          {shown.length === 0 && <p className="fs-none">Nothing matches "{text}".</p>}
          {shown.map((g) => (
            <div key={g.id} role="group" aria-labelledby={`${id}-g-${g.id}`} className="fs-group">
              <div id={`${id}-g-${g.id}`} className="fs-group-label">
                {g.label}
              </div>
              {g.values.map((v) => {
                const n = flat.findIndex((o) => o.group === g.id && o.value === v.id);
                const ticked = on(g.id, v.id);
                return (
                  <div
                    key={v.id}
                    id={`${id}-${n}`}
                    role="option"
                    aria-selected={ticked}
                    className="fs-option"
                    data-active={n === active || undefined}
                    data-empty={v.count === 0 && !ticked ? true : undefined}
                    // Keep the focus in the field, as a select keeps it.
                    onPointerDown={(e) => e.preventDefault()}
                    onPointerEnter={() => setActive(n)}
                    onClick={() => toggle(g.id, v.id)}
                  >
                    <span className="fs-box" aria-hidden="true">
                      {ticked && <Check size={11} strokeWidth={3} />}
                    </span>
                    <span className="fs-label">{v.label}</span>
                    {v.count !== undefined && <span className="fs-count">{v.count.toLocaleString()}</span>}
                  </div>
                );
              })}
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
