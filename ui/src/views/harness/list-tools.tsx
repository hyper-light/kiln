/**
 * How Mission control's list is shown: one filter field (names as you type; runner, harness,
 * state, folder, model and mode as checkboxes with counts) and a sort select beside it. The list
 * holds still while agents work: it is taken when you change the filter or the sort (or refresh
 * it), new agents come in on top, and the list says when it no longer matches what's chosen.
 */
import * as React from "react";
import type { AgentInfo } from "@/api/harness";
import { FacetBox, type Chosen, type Facet } from "@/components/ui/facet-box";
import { SelectField } from "@/components/ui/misc";
import { modelLabel, tildify } from "./derive";
import { machineName } from "./parts";

export type SortId = "newest" | "oldest" | "active" | "name" | "name-desc" | "runner" | "harness" | "state" | "cost";

export interface ListView {
  sort: SortId;
  /** Chosen options, by facet. */
  facets: Chosen;
}
export const DEFAULT_VIEW: ListView = { sort: "newest", facets: {} };

const SORTS: { value: SortId; label: string }[] = [
  { value: "newest", label: "Newest first" },
  { value: "oldest", label: "Oldest first" },
  { value: "active", label: "Recently active" },
  { value: "name", label: "Name, A to Z" },
  { value: "name-desc", label: "Name, Z to A" },
  { value: "runner", label: "Runner" },
  { value: "harness", label: "Harness" },
  { value: "state", label: "State, needs you first" },
  { value: "cost", label: "Cost, highest first" },
];

/** Where an agent stands, in Mission control's words; sorting by state follows this order. */
export const STANDS = [
  { id: "needs", label: "Needs you" },
  { id: "crashed", label: "Stopped" },
  { id: "paused", label: "Paused" },
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

const collator = new Intl.Collator(undefined, {
  numeric: true,
  sensitivity: "base",
});
const folder = (cwd: string) => cwd.replace(/\/+$/, "").split("/").pop() || cwd;
const modeLabel = (m: string) => {
  const words = m
    .replace(/([a-z])([A-Z])/g, "$1 $2")
    .replace(/[_-]+/g, " ")
    .toLowerCase();
  return words.charAt(0).toUpperCase() + words.slice(1);
};

/** The dimensions you can filter by: what each agent has, and what to call it. */
const DIMENSIONS: {
  id: string;
  label: string;
  of: (r: ViewRow) => string;
  name: (v: string, rows: ViewRow[]) => string;
  title?: (v: string) => string;
  always?: boolean;
}[] = [
  {
    id: "state",
    label: "State",
    of: (r) => r.stand,
    name: (v) => STANDS.find((s) => s.id === v)?.label ?? v,
    always: true,
  },
  {
    id: "runner",
    label: "Runner",
    of: (r) => machineName(r.a.target),
    name: (v) => v,
  },
  {
    id: "harness",
    label: "Harness",
    of: (r) => r.a.kind,
    name: (v, rows) => rows.find((r) => r.a.kind === v)?.a.kind_name ?? v,
  },
  {
    id: "folder",
    label: "Folder",
    of: (r) => r.a.cwd,
    name: (v) => folder(v),
    title: (v) => tildify(v),
  },
  {
    id: "model",
    label: "Model",
    of: (r) => r.a.model,
    name: (v) => modelLabel(v),
  },
  {
    id: "mode",
    label: "Permissions",
    of: (r) => r.a.mode,
    name: (v) => modeLabel(v),
  },
];

function sortBy(sort: SortId): (x: ViewRow, y: ViewRow) => number {
  switch (sort) {
    case "newest":
      return (x, y) => y.a.created_at - x.a.created_at;
    case "oldest":
      return (x, y) => x.a.created_at - y.a.created_at;
    case "active":
      return (x, y) => y.a.last_activity - x.a.last_activity;
    case "name":
      return (x, y) => collator.compare(x.a.name, y.a.name);
    case "name-desc":
      return (x, y) => collator.compare(y.a.name, x.a.name);
    case "runner":
      return (x, y) => collator.compare(machineName(x.a.target), machineName(y.a.target));
    case "harness":
      return (x, y) => collator.compare(x.a.kind_name, y.a.kind_name);
    case "state":
      return (x, y) => (RANK.get(x.stand) ?? STANDS.length) - (RANK.get(y.stand) ?? STANDS.length);
    case "cost":
      return (x, y) => (y.a.cost_usd ?? 0) - (x.a.cost_usd ?? 0);
  }
}

export function sortRows<T extends ViewRow>(rows: T[], sort: SortId): T[] {
  const by = sortBy(SORTS.some((s) => s.value === sort) ? sort : "newest");
  return [...rows].sort((x, y) => by(x, y) || y.a.created_at - x.a.created_at || x.a.id.localeCompare(y.a.id));
}

/** Matches the typed words (its name or title) and every facet chosen (any of a facet's options). */
export function matches(q: string, facets: Chosen, r: ViewRow, except?: string): boolean {
  const words = q.trim().toLowerCase();
  if (words && !r.a.name.toLowerCase().includes(words) && !(r.a.title ?? "").toLowerCase().includes(words)) return false;
  return DIMENSIONS.every((d) => d.id === except || !facets[d.id]?.length || facets[d.id]!.includes(d.of(r)));
}

export const isFiltered = (q: string, facets: Chosen) => Boolean(q.trim()) || Object.values(facets).some((v) => v.length > 0);

/** Each dimension's options with how many agents each would show, the other choices as they are. */
function facetsOf(rows: ViewRow[], q: string, chosen: Chosen): Facet[] {
  return DIMENSIONS.flatMap((d) => {
    const seen = new Map<string, number>();
    for (const r of rows) {
      const v = d.of(r);
      if (v && !seen.has(v)) seen.set(v, 0);
    }
    for (const r of rows) {
      const v = d.of(r);
      if (v && matches(q, chosen, r, d.id)) seen.set(v, (seen.get(v) ?? 0) + 1);
    }
    const picked = chosen[d.id] ?? [];
    // A dimension all agents share says nothing; it shows once two differ (or one is chosen).
    if (seen.size < 2 && picked.length === 0 && !d.always) return [];
    const options = [...seen.entries()].map(([id, count]) => ({
      id,
      label: d.name(id, rows),
      count,
      title: d.title?.(id),
    }));
    if (d.id === "state") options.sort((x, y) => (RANK.get(x.id) ?? 99) - (RANK.get(y.id) ?? 99));
    else options.sort((x, y) => collator.compare(x.label, y.label));
    return [{ id: d.id, label: d.label, options }];
  });
}

/** The filter field and the sort, side by side (stacked on a phone). */
export function ListBar({ rows, q, onQuery, view, onView }: { rows: ViewRow[]; q: string; onQuery: (q: string) => void; view: ListView; onView: (v: ListView) => void }) {
  const facets = React.useMemo(() => facetsOf(rows, q, view.facets), [rows, q, view.facets]);
  return (
    <div className="ms-bar">
      <FacetBox
        className="ms-filter"
        label="Filter agents"
        placeholder="Filter by name, runner, harness, state…"
        query={q}
        onQuery={onQuery}
        facets={facets}
        chosen={view.facets}
        onChosen={(facets) => onView({ ...view, facets })}
      />
      <SelectField className="ms-sort" label="Sort agents" hideLabel value={view.sort} onChange={(v) => onView({ ...view, sort: v as SortId })} options={SORTS} />
    </div>
  );
}

interface Taken {
  key: string;
  /** In order: what showed, each in the list or among the ended. */
  ids: string[];
  ended: ReadonlySet<string>;
  /** Every agent there was: one not among them is new. */
  known: ReadonlySet<string>;
}

/**
 * The list as shown: what the filter and sort gave when last applied, with each agent as it is now
 * (an agent that no longer matches stays until then, its words saying where it stands). New agents
 * come in on top. `stale`: applying them again would show something else.
 */
export function useShownList<T extends ViewRow>(all: T[], q: string, view: ListView, loaded: boolean) {
  const now = React.useMemo(
    () =>
      sortRows(
        all.filter((r) => matches(q, view.facets, r)),
        view.sort,
      ),
    [all, q, view],
  );
  const [tick, setTick] = React.useState(0);
  const key = `${JSON.stringify(view)}|${q}|${loaded ? 1 : 0}|${tick}`;
  const [taken, setTaken] = React.useState<Taken | null>(null);
  let t = taken;
  if (!t || t.key !== key) {
    t = {
      key,
      ids: now.map((r) => r.a.id),
      ended: new Set(now.filter((r) => r.stand === "ended").map((r) => r.a.id)),
      known: new Set(all.map((r) => r.a.id)),
    };
    setTaken(t);
  }
  const shown = React.useMemo(() => {
    const byId = new Map(all.map((r) => [r.a.id, r]));
    const fresh = now.filter((r) => !t.known.has(r.a.id));
    const kept = t.ids.map((id) => byId.get(id)).filter((r): r is T => r !== undefined);
    return {
      live: [...fresh.filter((r) => r.stand !== "ended"), ...kept.filter((r) => !t.ended.has(r.a.id))],
      ended: [...fresh.filter((r) => r.stand === "ended"), ...kept.filter((r) => t.ended.has(r.a.id))],
    };
  }, [all, now, t]);
  const stale = React.useMemo(() => {
    const live = now.filter((r) => r.stand !== "ended").map((r) => r.a.id);
    const ended = now.filter((r) => r.stand === "ended").map((r) => r.a.id);
    return live.join() !== shown.live.map((r) => r.a.id).join() || ended.join() !== shown.ended.map((r) => r.a.id).join();
  }, [now, shown]);
  const refresh = React.useCallback(() => setTick((n) => n + 1), []);
  return { ...shown, stale, refresh, matching: now.length };
}
