/**
 * Attention plan 01a199e1b177752e84dad5dc990e6a15.
 * How Mission control's list is shown: one filter field (names as you type; runner, harness,
 * state, folder, model and mode as checkboxes with counts) and a sort select beside it. By default
 * the agents that need you come first, as they come to: the list follows them while they work,
 * and holds still while you use it (useListHold), its rows then gliding to their places (useGlide).
 */
import * as React from "react";
import type { AgentInfo } from "@/api/harness";
import { activityAt } from "./mission-rows";
import { FacetBox, type Chosen, type Facet } from "@/components/ui/facet-box";
import { FilterBar, SelectField } from "@/components/ui/misc";
import { labelOf, useLabels, type Labels } from "@/lib/data";
import { modelLabel, tildify } from "./derive";
import { machineName, machineShown } from "./parts";

export type SortId = "priority" | "yours" | "newest" | "oldest" | "active" | "name" | "name-desc" | "runner" | "harness" | "cost";

export interface ListView {
  sort: SortId;
  /** Chosen options, by facet. */
  facets: Chosen;
}
export const DEFAULT_VIEW: ListView = { sort: "priority", facets: {} };

/** Needs you first, then the order learned from what you do; "yours" opts out of the learning (attention plan 01a199e1b177752e84dad5dc990e6a15). */
const SORTS: { value: SortId; label: string }[] = [
  { value: "priority", label: "Suggested" },
  { value: "yours", label: "Last Messaged" },
  { value: "newest", label: "Newest" },
  { value: "oldest", label: "Oldest" },
  { value: "active", label: "Last active" },
  { value: "name", label: "Name A–Z" },
  { value: "name-desc", label: "Name Z–A" },
  { value: "runner", label: "Runner" },
  { value: "harness", label: "Harness" },
  { value: "cost", label: "Highest cost" },
];

/** Where an agent stands, in Mission control's words (the State filter lists them so). */
export const STANDS = [
  { id: "needs", label: "Needs you" },
  { id: "working", label: "Working" },
  { id: "ready", label: "Ready" },
  { id: "finished", label: "Finished" },
  { id: "crashed", label: "Stopped" },
  { id: "paused", label: "Paused" },
  { id: "offline", label: "Offline" },
  { id: "ended", label: "Ended" },
  { id: "archived", label: "Archived" },
];
const RANK = new Map(STANDS.map((s, i) => [s.id, i]));

/** Since when it has waited on you: its oldest request (or, without one, when it paused its goals for you, or its last word). */
const waitingSince = (a: AgentInfo) => a.approvals.reduce((at, p) => Math.min(at, p.at || at), a.approvals.length > 0 ? Infinity : (a.held?.since ?? a.last_turn?.at ?? 0));

export interface ViewRow {
  a: AgentInfo;
  stand: string;
  rank?: number;
  pinned?: number;
  requestRank?: number;
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
  name: (v: string, rows: ViewRow[], labels: Labels) => string;
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
    name: (v, _rows, labels) => labelOf(v, labels),
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

/** Unanswered requests and stopped agents require intervention regardless of their learned rank. */
export const intervention = (row: ViewRow) => (row.stand === "needs" ? 0 : row.stand === "crashed" ? 1 : 2);

/** The server orders the whole pending set; mixing unlike known/unknown ratios here would be inconsistent. */
function compareIntervention(x: ViewRow, y: ViewRow): number {
  if (intervention(x) === 2 && intervention(y) === 2) return 0;
  if (intervention(x) === 2 || intervention(y) === 2) return intervention(x) - intervention(y);
  const rank = (x.requestRank ?? Number.MAX_SAFE_INTEGER) - (y.requestRank ?? Number.MAX_SAFE_INTEGER);
  if (rank) return rank;
  const kind = intervention(x) - intervention(y);
  if (kind) return kind;
  return x.stand === "crashed" ? (x.a.exit?.at ?? 0) - (y.a.exit?.at ?? 0) : waitingSince(x.a) - waitingSince(y.a);
}

function sortBy(sort: SortId, labels?: Labels): (x: ViewRow, y: ViewRow) => number {
  switch (sort) {
    case "priority":
    case "yours":
      return (x, y) => {
        if (intervention(x) < 2 || intervention(y) < 2) {
          return compareIntervention(x, y);
        }
        if (x.pinned !== undefined || y.pinned !== undefined) {
          if (x.pinned === undefined) return 1;
          if (y.pinned === undefined) return -1;
          return x.pinned - y.pinned;
        }
        return (x.rank ?? Number.MAX_SAFE_INTEGER) - (y.rank ?? Number.MAX_SAFE_INTEGER);
      };
    case "newest":
      return (x, y) => y.a.created_at - x.a.created_at;
    case "oldest":
      return (x, y) => x.a.created_at - y.a.created_at;
    case "active":
      return (x, y) => activityAt(y.a) - activityAt(x.a);
    case "name":
      return (x, y) => collator.compare(x.a.name, y.a.name);
    case "name-desc":
      return (x, y) => collator.compare(y.a.name, x.a.name);
    case "runner":
      return (x, y) => collator.compare(machineShown(x.a.target, labels), machineShown(y.a.target, labels));
    case "harness":
      return (x, y) => collator.compare(x.a.kind_name, y.a.kind_name);
    case "cost":
      return (x, y) => (y.a.cost_usd ?? 0) - (x.a.cost_usd ?? 0);
  }
}

/** A sort kept from before (one since gone, like "state") is the default. */
export const sortOf = (sort: string): SortId => (SORTS.some((s) => s.value === sort) ? (sort as SortId) : DEFAULT_VIEW.sort);

/** In the sort's order; a sort chosen on purpose (by name, by age…) is followed as it is (by runner: by the names they're shown by). */
export function sortRows<T extends ViewRow>(rows: T[], sort: SortId, labels?: Labels): T[] {
  const by = sortBy(sortOf(sort), labels);
  return [...rows].sort((x, y) => by(x, y) || x.a.id.localeCompare(y.a.id));
}

/** Matches the typed words (its name or title) and every facet chosen (any of a facet's options). An archived chat shows only where State: Archived is chosen. */
export function matches(q: string, facets: Chosen, r: ViewRow, except?: string): boolean {
  if (r.stand === "archived" && except !== "state" && !facets.state?.includes("archived")) return false;
  const words = q.trim().toLowerCase();
  if (words && !r.a.name.toLowerCase().includes(words) && !(r.a.title ?? "").toLowerCase().includes(words)) return false;
  return DIMENSIONS.every((d) => d.id === except || !facets[d.id]?.length || facets[d.id]!.includes(d.of(r)));
}

export const isFiltered = (q: string, facets: Chosen) => Boolean(q.trim()) || Object.values(facets).some((v) => v.length > 0);

/** Each dimension's options with how many agents each would show, the other choices as they are (single O(N) pass). */
function facetsOf(rows: ViewRow[], q: string, chosen: Chosen, labels: Labels): Facet[] {
  const words = q.trim().toLowerCase();
  const statePicked = chosen.state;
  const showArchived = statePicked?.includes("archived") ?? false;
  const dimSets = DIMENSIONS.map((d) => {
    const picked = chosen[d.id];
    return picked && picked.length > 0 ? new Set(picked) : null;
  });
  const counts = DIMENSIONS.map(() => new Map<string, number>());
  const kindNames = new Map<string, string>();
  const nDims = DIMENSIONS.length;
  for (let ri = 0; ri < rows.length; ri++) {
    const r = rows[ri]!;
    if (r.a.kind && r.a.kind_name && !kindNames.has(r.a.kind)) kindNames.set(r.a.kind, r.a.kind_name);
    const vals: string[] = new Array<string>(nDims);
    let mismatches = 0;
    let mismatchAt = -1;
    for (let di = 0; di < nDims; di++) {
      const v = DIMENSIONS[di]!.of(r);
      vals[di] = v;
      if (v && !counts[di]!.has(v)) counts[di]!.set(v, 0);
      const set = dimSets[di];
      if (set && !set.has(v)) {
        mismatches++;
        mismatchAt = di;
      }
    }
    const textOk = !words || r.a.name.toLowerCase().includes(words) || (r.a.title ?? "").toLowerCase().includes(words);
    if (!textOk || mismatches > 1) continue;
    const isArchived = r.stand === "archived";
    if (mismatches === 0) {
      for (let di = 0; di < nDims; di++) {
        if (isArchived && di !== 0 && !showArchived) continue;
        const v = vals[di]!;
        if (v) counts[di]!.set(v, (counts[di]!.get(v) ?? 0) + 1);
      }
    } else if (mismatchAt >= 0) {
      if (isArchived && mismatchAt !== 0 && !showArchived) continue;
      const v = vals[mismatchAt]!;
      if (v) counts[mismatchAt]!.set(v, (counts[mismatchAt]!.get(v) ?? 0) + 1);
    }
  }
  const out: Facet[] = [];
  for (let di = 0; di < nDims; di++) {
    const d = DIMENSIONS[di]!;
    const seen = counts[di]!;
    const picked = chosen[d.id] ?? [];
    if (seen.size < 2 && picked.length === 0 && !d.always) continue;
    const options = [...seen.entries()].map(([id, count]) => ({
      id,
      label: d.id === "harness" ? (kindNames.get(id) ?? id) : d.name(id, rows, labels),
      count,
      title: d.title?.(id),
    }));
    if (d.id === "state") options.sort((x, y) => (RANK.get(x.id) ?? 99) - (RANK.get(y.id) ?? 99));
    else options.sort((x, y) => collator.compare(x.label, y.label));
    out.push({ id: d.id, label: d.label, options });
  }
  return out;
}

/** The filter field and the sort, side by side (on a phone too). */
export function ListBar({ rows, q, onQuery, view, onView }: { rows: ViewRow[]; q: string; onQuery: (q: string) => void; view: ListView; onView: (v: ListView) => void }) {
  // Runners are named by their labels: the options change as those do.
  const labels = useLabels();
  const facets = React.useMemo(() => facetsOf(rows, q, view.facets, labels), [rows, q, view.facets, labels]);
  return (
    <FilterBar className="ms-bar">
      <FacetBox
        label="Filter agents"
        placeholder="Filter by name, runner, harness, state…"
        query={q}
        onQuery={onQuery}
        facets={facets}
        chosen={view.facets}
        onChosen={(facets) => onView({ ...view, facets })}
      />
      <SelectField label="Sort agents" hideLabel value={view.sort} onChange={(v) => onView({ ...view, sort: v as SortId })} options={SORTS} />
    </FilterBar>
  );
}

/** What showed when the list began to be held: in order, each in the list or among the ended. */
interface Kept {
  ids: string[];
  ended: ReadonlySet<string>;
}

/**
 * The list as shown: what the filter and sort give, with each agent as it is now, so one that comes
 * to need you rises to the top as it does. While the list is `held` (in use: useListHold) its rows
 * keep their places, an agent that no longer matches included, and new agents come in on top; then
 * it follows again.
 */
export function useShownList<T extends ViewRow>(all: T[], q: string, view: ListView, held: boolean, reset = "", acted: ReadonlySet<string> = NONE) {
  // By runner is by the names they're shown by.
  const labels = useLabels();
  const now = React.useMemo(
    () =>
      sortRows(
        all.filter((r) => matches(q, view.facets, r)),
        view.sort,
        labels,
      ),
    [all, q, view, labels],
  );
  // Choosing another filter or sort is using the list too, but it asks for that order at once.
  const chosen = `${JSON.stringify(view)}|${q}|${reset}`;
  const [kept, setKept] = React.useState<{ chosen: string; was: Kept } | null>(null);
  if (held && (!kept || kept.chosen !== chosen)) {
    setKept({ chosen, was: { ids: now.map((r) => r.a.id), ended: new Set(now.filter((r) => r.stand === "ended").map((r) => r.a.id)) } });
  } else if (!held && kept) setKept(null);
  const was = held && kept?.chosen === chosen ? kept.was : null;
  return React.useMemo(() => heldRows(all, now, was, view.sort === "priority" || view.sort === "yours", acted), [all, now, was, view.sort, acted]);
}
const NONE: ReadonlySet<string> = new Set();

/**
 * Keep familiar positions, but never leave a newly urgent row in the folded Ended list. The rows you
 * acted on (`acted`) aren't held: each takes its place as the list now has it, just before the first
 * held row the list now puts after it, and every other row stays where it was.
 */
export function heldRows<T extends ViewRow>(all: T[], now: T[], was: Kept | null, prioritized = true, acted: ReadonlySet<string> = NONE) {
  if (!was) return { live: now.filter((r) => r.stand !== "ended"), ended: now.filter((r) => r.stand === "ended"), matching: now.length };
  const byId = new Map(all.map((r) => [r.a.id, r]));
  const known = new Set(was.ids);
  // New rows come in on top; one you brought back (unparked, restored) is yours, placed as below.
  const fresh = now.filter((r) => !known.has(r.a.id) && !acted.has(r.a.id));
  const stayed = was.ids.flatMap((id) => {
    const r = acted.has(id) ? undefined : byId.get(id);
    return r ? [r] : [];
  });
  const at = new Map(now.map((r, i) => [r.a.id, i]));
  for (const r of now) {
    const mine = at.get(r.a.id);
    if (!acted.has(r.a.id) || mine === undefined) continue;
    const before = stayed.findIndex((other) => (at.get(other.a.id) ?? Number.POSITIVE_INFINITY) > mine);
    stayed.splice(before < 0 ? stayed.length : before, 0, r);
  }
  const ended = (r: T) => (acted.has(r.a.id) || !known.has(r.a.id) ? r.stand === "ended" : was.ended.has(r.a.id) && intervention(r) === 2);
  return {
    live: [...fresh.filter((r) => r.stand !== "ended"), ...stayed.filter((r) => !ended(r))].sort((x, y) => (prioritized ? compareIntervention(x, y) : 0)),
    ended: [...fresh.filter((r) => r.stand === "ended"), ...stayed.filter(ended)],
    matching: now.length,
  };
}

export { OPEN, useGlide, useListHold, type Glided } from "@/lib/list-hold";
