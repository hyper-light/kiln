/**
 * Mission control's list, sorted and filtered the way you ask: by when each started, when it last
 * did something, its name, runner, harness, where it stands or what it cost; and only the ones
 * whose name, runner, harness or state you pick. One line says how the list is shown and opens in
 * place to change it. A sort is taken when you choose it (or ask again): rows never reorder on
 * their own while agents update; the line offers "Re-sort" once the order has gone stale.
 */
import * as React from "react";
import { ChevronRight, RotateCcw, Search, X } from "lucide-react";
import type { AgentInfo } from "@/api/harness";
import { cn } from "@/lib/utils";
import { Facet } from "./broadcast";
import { machineName } from "./parts";

export type SortKey = "created" | "active" | "name" | "runner" | "harness" | "state" | "cost";
export type Dir = "asc" | "desc";
export interface View {
  sort: SortKey;
  dir: Dir;
  q: string;
  runners: string[];
  kinds: string[];
  stands: string[];
}
export const DEFAULT_VIEW: View = { sort: "created", dir: "desc", q: "", runners: [], kinds: [], stands: [] };

const SORTS: { id: SortKey; label: string; dir: Dir }[] = [
  { id: "created", label: "Created", dir: "desc" },
  { id: "active", label: "Last active", dir: "desc" },
  { id: "name", label: "Name", dir: "asc" },
  { id: "runner", label: "Runner", dir: "asc" },
  { id: "harness", label: "Harness", dir: "asc" },
  { id: "state", label: "State", dir: "asc" },
  { id: "cost", label: "Cost", dir: "desc" },
];

/** Where an agent stands, in Mission control's words; "State" sorts in this order. */
export const STANDS = [
  { id: "needs", label: "Needs you" },
  { id: "crashed", label: "Stopped" },
  { id: "working", label: "Working" },
  { id: "finished", label: "Finished" },
  { id: "ready", label: "Ready" },
  { id: "offline", label: "Offline" },
  { id: "ended", label: "Ended" },
];
const RANK = new Map(STANDS.map((s, i) => [s.id, i]));

export interface ViewRow {
  a: AgentInfo;
  stand: string;
}

const collator = new Intl.Collator(undefined, { numeric: true, sensitivity: "base" });

export function sortRows<T extends ViewRow>(rows: T[], sort: SortKey, dir: Dir): T[] {
  const by = (x: T, y: T): number => {
    switch (sort) {
      case "created":
        return x.a.created_at - y.a.created_at;
      case "active":
        return x.a.last_activity - y.a.last_activity;
      case "name":
        return collator.compare(x.a.name, y.a.name);
      case "runner":
        return collator.compare(machineName(x.a.target), machineName(y.a.target));
      case "harness":
        return collator.compare(x.a.kind_name, y.a.kind_name);
      case "state":
        return (RANK.get(x.stand) ?? STANDS.length) - (RANK.get(y.stand) ?? STANDS.length);
      case "cost":
        return (x.a.cost_usd ?? 0) - (y.a.cost_usd ?? 0);
    }
  };
  const sign = dir === "asc" ? 1 : -1;
  return [...rows].sort((x, y) => sign * by(x, y) || y.a.created_at - x.a.created_at || x.a.id.localeCompare(y.a.id));
}

export function matches(view: View, r: ViewRow): boolean {
  const q = view.q.trim().toLowerCase();
  return (
    (!q || r.a.name.toLowerCase().includes(q) || (r.a.title ?? "").toLowerCase().includes(q)) &&
    (view.runners.length === 0 || view.runners.includes(machineName(r.a.target))) &&
    (view.kinds.length === 0 || view.kinds.includes(r.a.kind)) &&
    (view.stands.length === 0 || view.stands.includes(r.stand))
  );
}

export const isFiltered = (v: View) => Boolean(v.q.trim() || v.runners.length || v.kinds.length || v.stands.length);

export function ListTools({
  view,
  onView,
  rows,
  stale,
  onResort,
  shown,
}: {
  view: View;
  onView: (v: View) => void;
  /** Every agent (for the choices offered). */
  rows: ViewRow[];
  /** The order taken for this sort no longer matches it. */
  stale: boolean;
  onResort: () => void;
  /** How many the filters leave. */
  shown: number;
}) {
  const [open, setOpen] = React.useState(false);
  const runners = [...new Set(rows.map((r) => machineName(r.a.target)))].sort(collator.compare);
  const kinds = [...new Map(rows.map((r) => [r.a.kind, r.a.kind_name])).entries()];
  const arrow = view.dir === "asc" ? "↑" : "↓";
  const summary = [
    `${SORTS.find((s) => s.id === view.sort)?.label ?? view.sort} ${arrow}`,
    view.runners.length ? view.runners.join(" + ") : "all runners",
    view.kinds.length ? view.kinds.map((k) => kinds.find(([id]) => id === k)?.[1] ?? k).join(" + ") : "any harness",
    view.stands.length ? view.stands.map((s) => STANDS.find((x) => x.id === s)?.label ?? s).join(" + ") : "any state",
  ].join(" · ");
  const setSort = (id: SortKey) => {
    const s = SORTS.find((x) => x.id === id);
    if (!s) return;
    onView(view.sort === id ? { ...view, dir: view.dir === "asc" ? "desc" : "asc" } : { ...view, sort: id, dir: s.dir });
  };
  const filtered = isFiltered(view);
  return (
    <div className="ms-tools">
      <div className="ms-tools-line">
        <label className="ms-find">
          <Search size={13} aria-hidden="true" />
          <input value={view.q} onChange={(e) => onView({ ...view, q: e.target.value })} placeholder="Find by name" aria-label="Find agents by name" />
          {view.q && (
            <button type="button" aria-label="Clear the name" onClick={() => onView({ ...view, q: "" })}>
              <X size={12} />
            </button>
          )}
        </label>
        <button type="button" className="ms-bc-summary ms-tools-summary" aria-expanded={open} onClick={() => setOpen(!open)}>
          <ChevronRight size={12} className={cn("ms-chev", open && "rotate-90")} aria-hidden="true" />
          {summary}
        </button>
        <span className="ms-tools-end">
          {stale && (
            <button type="button" className="ms-tools-resort" onClick={onResort}>
              <RotateCcw size={11} aria-hidden="true" /> Re-sort
            </button>
          )}
          {filtered && (
            <span className="ms-tools-count">
              {shown} of {rows.length}
            </span>
          )}
        </span>
      </div>
      {open && (
        <div className="ms-bc-facets ms-tools-facets">
          <div className="ms-bc-facet" role="group" aria-label="Sort by">
            <span className="ms-bc-facet-label">Sort</span>
            {SORTS.map((s) => (
              <button key={s.id} type="button" className="ms-bc-chip" aria-pressed={view.sort === s.id} onClick={() => setSort(s.id)}>
                {s.label}
                {view.sort === s.id && <b className="ms-tools-dir">{arrow}</b>}
              </button>
            ))}
          </div>
          <Facet label="Runner" options={runners.map((m) => ({ id: m, label: m }))} chosen={view.runners} onChange={(runners) => onView({ ...view, runners })} />
          <Facet label="Harness" options={kinds.map(([id, label]) => ({ id, label }))} chosen={view.kinds} onChange={(kinds) => onView({ ...view, kinds })} />
          <Facet label="State" options={STANDS} chosen={view.stands} onChange={(stands) => onView({ ...view, stands })} />
          {filtered && (
            <button type="button" className="ms-link ms-tools-clear" onClick={() => onView({ ...DEFAULT_VIEW, sort: view.sort, dir: view.dir })}>
              Clear filters
            </button>
          )}
        </div>
      )}
    </div>
  );
}
