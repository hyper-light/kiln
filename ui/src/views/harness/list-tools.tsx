/**
 * How Mission control's list is shown: one filter field (names as you type; runner, harness,
 * state, folder, model and mode as checkboxes with counts) and a sort select beside it. By default
 * the agents that need you come first, as they come to: the list follows them while they work,
 * and holds still while you use it (useListHold), its rows then gliding to their places (useGlide).
 */
import * as React from "react";
import type { AgentInfo } from "@/api/harness";
import { FacetBox, type Chosen, type Facet } from "@/components/ui/facet-box";
import { FilterBar, SelectField } from "@/components/ui/misc";
import { modelLabel, tildify } from "./derive";
import { machineName } from "./parts";

export type SortId = "priority" | "newest" | "oldest" | "active" | "name" | "name-desc" | "runner" | "harness" | "cost";

export interface ListView {
  sort: SortId;
  /** Chosen options, by facet. */
  facets: Chosen;
}
export const DEFAULT_VIEW: ListView = { sort: "priority", facets: {} };

const SORTS: { value: SortId; label: string }[] = [
  { value: "priority", label: "Needs you first" },
  { value: "newest", label: "Newest first" },
  { value: "oldest", label: "Oldest first" },
  { value: "active", label: "Recently active" },
  { value: "name", label: "Name, A to Z" },
  { value: "name-desc", label: "Name, Z to A" },
  { value: "runner", label: "Runner" },
  { value: "harness", label: "Harness" },
  { value: "cost", label: "Cost, highest first" },
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
];
const RANK = new Map(STANDS.map((s, i) => [s.id, i]));

/** Since when it has waited on you: its oldest request (or, without one, when it paused its goals for you, or its last word). */
const waitingSince = (a: AgentInfo) => a.approvals.reduce((at, p) => Math.min(at, p.at || at), a.approvals.length > 0 ? Infinity : (a.held?.since ?? a.last_activity));

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
    case "priority":
      return (x, y) => {
        // Needs you, working, ready, finished, then inactive states — also the filter's order.
        const by = (RANK.get(x.stand) ?? RANK.size) - (RANK.get(y.stand) ?? RANK.size);
        if (by !== 0) return by;
        // Waiting on you the longest first; the latest news first; otherwise the newest.
        if (x.stand === "needs") return waitingSince(x.a) - waitingSince(y.a);
        if (x.stand === "crashed" || x.stand === "paused" || x.stand === "finished") return y.a.last_activity - x.a.last_activity;
        return y.a.created_at - x.a.created_at;
      };
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
    case "cost":
      return (x, y) => (y.a.cost_usd ?? 0) - (x.a.cost_usd ?? 0);
  }
}

/** A sort kept from before (one since gone, like "state") is the default. */
export const sortOf = (sort: string): SortId => (SORTS.some((s) => s.value === sort) ? (sort as SortId) : DEFAULT_VIEW.sort);

/** In the sort's order; a sort chosen on purpose (by name, by age…) is followed as it is. */
export function sortRows<T extends ViewRow>(rows: T[], sort: SortId): T[] {
  const by = sortBy(sortOf(sort));
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

/** The filter field and the sort, side by side (on a phone too). */
export function ListBar({ rows, q, onQuery, view, onView }: { rows: ViewRow[]; q: string; onQuery: (q: string) => void; view: ListView; onView: (v: ListView) => void }) {
  const facets = React.useMemo(() => facetsOf(rows, q, view.facets), [rows, q, view.facets]);
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
export function useShownList<T extends ViewRow>(all: T[], q: string, view: ListView, held: boolean) {
  const now = React.useMemo(
    () =>
      sortRows(
        all.filter((r) => matches(q, view.facets, r)),
        view.sort,
      ),
    [all, q, view],
  );
  // Choosing another filter or sort is using the list too, but it asks for that order at once.
  const chosen = `${JSON.stringify(view)}|${q}`;
  const [kept, setKept] = React.useState<{ chosen: string; was: Kept } | null>(null);
  if (held && (!kept || kept.chosen !== chosen)) {
    setKept({ chosen, was: { ids: now.map((r) => r.a.id), ended: new Set(now.filter((r) => r.stand === "ended").map((r) => r.a.id)) } });
  } else if (!held && kept) setKept(null);
  const was = held && kept?.chosen === chosen ? kept.was : null;
  return React.useMemo(() => {
    if (!was) return { live: now.filter((r) => r.stand !== "ended"), ended: now.filter((r) => r.stand === "ended"), matching: now.length };
    const byId = new Map(all.map((r) => [r.a.id, r]));
    const known = new Set(was.ids);
    const fresh = now.filter((r) => !known.has(r.a.id));
    const stayed = was.ids.map((id) => byId.get(id)).filter((r): r is T => r !== undefined);
    return {
      live: [...fresh.filter((r) => r.stand !== "ended"), ...stayed.filter((r) => !was.ended.has(r.a.id))],
      ended: [...fresh.filter((r) => r.stand === "ended"), ...stayed.filter((r) => was.ended.has(r.a.id))],
      matching: now.length,
    };
  }, [all, now, was]);
}

/** After the pointer last moved over the list (or a finger left it), this long before it moves again. */
const QUIET_MS = 1200;
/** While one of these is open (a row's menu, a dialog from it), the list holds still (one closing doesn't count). */
const OPEN = ':is([role="menu"], [role="dialog"], [role="alertdialog"]):not([data-state="closed"])';

/**
 * Whether the list is in use, so its rows hold still: a finger or button pressed on it, focus on one
 * of its controls, a menu or dialog open, or a moment after the pointer last moved over it.
 * `bind` goes on the element around the list.
 */
export function useListHold(extra = false) {
  const [hold] = React.useState(() => {
    let pressed = false;
    let focused = false;
    let moved = 0;
    let value = false;
    let timer = 0;
    const listeners = new Set<() => void>();
    const update = () => {
      window.clearTimeout(timer);
      const quiet = Date.now() - moved;
      const open = typeof document !== "undefined" && document.querySelector(OPEN) !== null;
      const next = pressed || focused || open || quiet < QUIET_MS;
      // Released by itself: once it's quiet (and checked again while something is open).
      if (next && !pressed && !focused) timer = window.setTimeout(update, open ? 500 : QUIET_MS - quiet + 20);
      if (next !== value) {
        value = next;
        listeners.forEach((l) => l());
      }
    };
    const touch = () => {
      moved = Date.now();
      if (!value) update();
    };
    const control = (el: EventTarget | null) => el instanceof Element && el.closest("button, a, input, textarea, select, [role='menuitem']") !== null;
    return {
      subscribe: (l: () => void) => {
        listeners.add(l);
        return () => listeners.delete(l);
      },
      get: () => value,
      bind: {
        // The page scrolling (a finger flinging it on, a wheel) is the list in use as well.
        ref: (el: HTMLElement | null) => {
          if (!el) return;
          window.addEventListener("scroll", touch, { passive: true });
          return () => {
            window.removeEventListener("scroll", touch);
            window.clearTimeout(timer);
          };
        },
        onPointerDown: () => {
          pressed = true;
          moved = Date.now();
          update();
        },
        onPointerUp: () => {
          pressed = false;
          moved = Date.now();
          update();
        },
        onPointerCancel: () => {
          pressed = false;
          moved = Date.now();
          update();
        },
        onPointerMove: touch,
        onWheel: touch,
        onPointerLeave: () => {
          pressed = false;
          update();
        },
        onFocus: (e: React.FocusEvent) => {
          focused = control(e.target) && (e.target as Element).matches(":focus-visible");
          update();
        },
        onBlur: (e: React.FocusEvent) => {
          focused = e.currentTarget.contains(e.relatedTarget as Node | null) && control(e.relatedTarget);
          moved = Date.now();
          update();
        },
      },
    };
  });
  const held = React.useSyncExternalStore(hold.subscribe, hold.get, () => false);
  return { held: held || extra, bind: hold.bind };
}

/** How long a row takes to glide to its new place. */
const GLIDE_MS = 260;

/** Where each gliding item is, from the top of the list it's in. */
function places(root: HTMLElement): Map<string, number> {
  const out = new Map<string, number>();
  for (const el of root.querySelectorAll<HTMLElement>("[data-glide]")) {
    const parent = el.parentElement;
    if (parent) out.set(el.dataset.glide ?? "", el.getBoundingClientRect().top - parent.getBoundingClientRect().top);
  }
  return out;
}

/**
 * Items (`data-glide="<key>"`) that glide to their new places when the order (`order`) changes,
 * from where they were, rather than jump; new ones fade in. Not when motion is paused here or by the
 * device. The ref goes on the element around them. While an item glides, it can't be pressed, so a
 * tap meant for what was there doesn't land on what slid under it.
 */
export function useGlide(order: string) {
  const [last] = React.useState(() => ({ order, root: null as HTMLElement | null, before: null as Map<string, number> | null }));
  if (last.order !== order) {
    // About to show another order: where everything is now (the page still shows the last one).
    last.order = order;
    last.before = last.root && document.documentElement.dataset.motion !== "paused" ? places(last.root) : null;
  }
  return React.useCallback(
    (el: HTMLElement | null) => {
      if (!el) return;
      last.root = el;
      const before = last.before;
      last.before = null;
      const off = () => {
        if (last.root === el) last.root = null;
      };
      if (!before) return off;
      for (const item of el.querySelectorAll<HTMLElement>("[data-glide]")) {
        const parent = item.parentElement;
        const was = before.get(item.dataset.glide ?? "");
        if (!parent) continue;
        if (was === undefined) {
          item.animate([{ opacity: 0 }, { opacity: 1 }], { duration: 180, easing: "ease-out" });
          continue;
        }
        const dy = was - (item.getBoundingClientRect().top - parent.getBoundingClientRect().top);
        if (Math.abs(dy) < 1) continue;
        item.style.pointerEvents = "none";
        const run = item.animate([{ transform: `translateY(${dy}px)` }, { transform: "none" }], { duration: GLIDE_MS, easing: "cubic-bezier(0.2, 0.8, 0.2, 1)" });
        const done = () => {
          item.style.pointerEvents = "";
        };
        run.onfinish = done;
        run.oncancel = done;
      }
      return off;
    },
    // A new ref for each order: React hands it the list once that order is on the page (and the
    // last one's cleanup runs first, so what was measured for this order is still there).
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [order],
  );
}
