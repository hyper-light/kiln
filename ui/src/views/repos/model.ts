/**
 * The Repositories page's list as data: one entry per repository kiln knows of (the ones it builds,
 * the synced ones and the ones machines hold, as GET /api/places joins them), what each is filtered
 * by (kind, machine, OS, architecture, labels, state) and sorted by. The same shape as the agents
 * list (views/harness/list-tools.tsx): one filter field whose facets count what each would show,
 * and a sort beside it.
 */
import type { Chosen, Facet } from "@/components/ui/facet-box";
import type { Place, PlaceMachine, PlaceTask, PlacesView, Repo, Run, SyncRepo, SyncView } from "@/api/types";

/** One repository, with everything the page shows of it. */
export interface Entry {
  key: string;
  name: string;
  /** A remote to clone it from. */
  origin: string | null;
  built: Repo | null;
  synced: SyncRepo | null;
  places: Place[];
  latest: Run | null;
  /** Its tasks, newest first. */
  tasks: PlaceTask[];
}

export type SortId = "name" | "name-desc" | "machines" | "machines-few" | "active" | "run" | "polled" | "added";

export interface ListView {
  sort: SortId;
  /** Chosen options, by facet. */
  facets: Chosen;
}
export const DEFAULT_VIEW: ListView = { sort: "name", facets: {} };

export const SORTS: { value: SortId; label: string }[] = [
  { value: "name", label: "Name, A to Z" },
  { value: "name-desc", label: "Name, Z to A" },
  { value: "machines", label: "Most machines" },
  { value: "machines-few", label: "Fewest machines" },
  { value: "active", label: "Recently active" },
  { value: "run", label: "Latest run" },
  { value: "polled", label: "Last polled" },
  { value: "added", label: "Recently added" },
];

const collator = new Intl.Collator(undefined, { numeric: true, sensitivity: "base" });

export const isActive = (t: PlaceTask) => t.state !== "done" && t.state !== "failed";

/** Every repository once: GET /api/places' rows joined to what /api/repos, /api/sync and the runs say. */
export function entriesOf(places: PlacesView | undefined, repos: Repo[] | undefined, sync: SyncView | undefined, runs: Run[] | undefined): Entry[] {
  const built = new Map((repos ?? []).map((r) => [r.id, r]));
  const synced = new Map((sync?.repos ?? []).map((s) => [s.id, s]));
  const latest = new Map<string, Run>();
  for (const r of runs ?? []) {
    const cur = latest.get(r.repo_id);
    if (!cur || r.created_at > cur.created_at) latest.set(r.repo_id, r);
  }
  const tasks = new Map<string, PlaceTask[]>();
  for (const t of places?.tasks ?? []) tasks.set(t.key, [...(tasks.get(t.key) ?? []), t]);
  const out: Entry[] = [];
  const seenBuilt = new Set<string>();
  const seenSynced = new Set<string>();
  for (const row of places?.repos ?? []) {
    const b = row.built ? (built.get(row.built) ?? null) : null;
    const s = row.synced ? (synced.get(row.synced) ?? null) : null;
    if (b) seenBuilt.add(b.id);
    if (s) seenSynced.add(s.id);
    out.push({ key: row.key, name: b?.name ?? row.name, origin: row.origin ?? b?.clone_url ?? null, built: b, synced: s, places: row.places, latest: b ? (latest.get(b.id) ?? null) : null, tasks: tasks.get(row.key) ?? [] });
  }
  // What the places haven't caught up with yet (or a server without them): by name, as before.
  for (const b of repos ?? []) {
    if (seenBuilt.has(b.id)) continue;
    const s = [...synced.values()].find((x) => !seenSynced.has(x.id) && !x.transient && sameName(b.name, x.name)) ?? null;
    if (s) seenSynced.add(s.id);
    out.push({ key: `built:${b.id}`, name: b.name, origin: b.clone_url, built: b, synced: s, places: s ? copyPlaces(s) : [], latest: latest.get(b.id) ?? null, tasks: [] });
  }
  for (const s of synced.values()) {
    if (seenSynced.has(s.id) || s.transient) continue;
    out.push({ key: `synced:${s.id}`, name: s.name, origin: null, built: null, synced: s, places: copyPlaces(s), latest: null, tasks: [] });
  }
  return out;
}

function sameName(built: string, synced: string) {
  const b = built.toLowerCase();
  const s = synced.toLowerCase();
  return b === s || b.split("/").pop() === s;
}

function copyPlaces(s: SyncRepo): Place[] {
  return s.copies.map((c) => ({ machine: c.machine, path: c.path, kind: "copy", git: null, copy: { repo: s.id, member: c.member, state: c.state, moving: false }, online: c.state !== "offline" }));
}

/** The machines a repository is on, each once. */
export const machinesOf = (e: Entry) => [...new Set(e.places.map((p) => p.machine))];

/** When anything last happened to it: git on a machine, a run, a poll. */
export function activeAt(e: Entry): number {
  const git = Math.max(0, ...e.places.map((p) => p.git?.active ?? 0));
  return Math.max(git, e.latest?.created_at ?? 0, e.synced?.created ?? 0);
}

const STATES = [
  { id: "under-way", label: "Under way" },
  { id: "conflicts", label: "Conflicts" },
  { id: "uncommitted", label: "Changes not committed" },
  { id: "unpushed", label: "Commits not pushed" },
  { id: "behind", label: "Behind its remote" },
  { id: "offline", label: "On an offline machine" },
  { id: "poll-error", label: "Poll error" },
];

const KINDS = [
  { id: "built", label: "Built by kiln" },
  { id: "synced", label: "Synced" },
  { id: "clone", label: "Cloned by kiln" },
  { id: "found", label: "Found on machines" },
  { id: "nowhere", label: "On no machine" },
];

/** What each entry has, for each dimension the list is filtered by. */
type Dims = Record<string, string[]>;

export function dimsOf(e: Entry, machines: Map<string, PlaceMachine>): Dims {
  const on = machinesOf(e);
  const ms = on.map((n) => machines.get(n)).filter((m): m is PlaceMachine => Boolean(m));
  const kind = [
    e.built && "built",
    e.synced && "synced",
    e.places.some((p) => p.kind === "clone" || (p.git?.kiln ?? false)) && "clone",
    e.places.some((p) => p.kind === "found") && "found",
    e.places.length === 0 && "nowhere",
  ].filter((x): x is string => Boolean(x));
  const state = [
    e.tasks.some(isActive) && "under-way",
    (e.synced?.conflicts.length ?? 0) > 0 && "conflicts",
    e.places.some((p) => (p.git?.changed ?? 0) > 0) && "uncommitted",
    e.places.some((p) => (p.git?.ahead ?? 0) > 0) && "unpushed",
    e.places.some((p) => (p.git?.behind ?? 0) > 0) && "behind",
    e.places.some((p) => !p.online) && "offline",
    e.built?.last_error && "poll-error",
  ].filter((x): x is string => Boolean(x));
  return {
    kind,
    machine: on,
    os: [...new Set(ms.map((m) => m.os))],
    arch: [...new Set(ms.map((m) => m.arch))],
    label: [...new Set(ms.flatMap((m) => m.labels))],
    state,
  };
}

const DIMENSIONS: { id: string; label: string; name: (v: string) => string; order?: string[]; always?: boolean }[] = [
  { id: "kind", label: "Kind", name: (v) => KINDS.find((k) => k.id === v)?.label ?? v, order: KINDS.map((k) => k.id), always: true },
  { id: "machine", label: "Machine", name: (v) => v },
  { id: "os", label: "OS", name: (v) => osLabel(v) },
  { id: "arch", label: "Architecture", name: (v) => v },
  { id: "label", label: "Machine labels", name: (v) => v },
  { id: "state", label: "State", name: (v) => STATES.find((s) => s.id === v)?.label ?? v, order: STATES.map((s) => s.id) },
];

export function osLabel(os: string) {
  return ({ macos: "macOS", linux: "Linux", windows: "Windows", freebsd: "FreeBSD" } as Record<string, string>)[os] ?? os;
}

/** Matches the words typed (its name, remote or a folder it is in) and every facet chosen (any of a facet's options). */
export function matches(q: string, chosen: Chosen, e: Entry, dims: Dims, except?: string): boolean {
  const words = q.trim().toLowerCase();
  if (words && ![e.name, e.key, e.origin ?? "", ...e.places.map((p) => p.path)].some((s) => s.toLowerCase().includes(words))) return false;
  return DIMENSIONS.every((d) => d.id === except || !chosen[d.id]?.length || (dims[d.id] ?? []).some((v) => chosen[d.id]!.includes(v)));
}

export const isFiltered = (q: string, chosen: Chosen) => Boolean(q.trim()) || Object.values(chosen).some((v) => v.length > 0);

/** Each dimension's options with how many repositories each would show, the other choices as they are. */
export function facetsOf(entries: Entry[], dims: Map<string, Dims>, q: string, chosen: Chosen): Facet[] {
  return DIMENSIONS.flatMap((d) => {
    const seen = new Map<string, number>();
    for (const e of entries) for (const v of dims.get(e.key)?.[d.id] ?? []) if (!seen.has(v)) seen.set(v, 0);
    for (const e of entries) {
      const ed = dims.get(e.key) ?? {};
      if (!matches(q, chosen, e, ed, d.id)) continue;
      for (const v of ed[d.id] ?? []) seen.set(v, (seen.get(v) ?? 0) + 1);
    }
    const picked = chosen[d.id] ?? [];
    // A dimension every repository shares says nothing; it shows once two differ (or one is chosen).
    if (seen.size < 2 && picked.length === 0 && !d.always) return [];
    const options = [...seen.entries()].map(([id, count]) => ({ id, label: d.name(id), count }));
    if (d.order) options.sort((x, y) => (d.order!.indexOf(x.id) + 1 || 99) - (d.order!.indexOf(y.id) + 1 || 99));
    else options.sort((x, y) => collator.compare(x.label, y.label));
    return [{ id: d.id, label: d.label, options }];
  });
}

function sortBy(sort: SortId): (x: Entry, y: Entry) => number {
  switch (sort) {
    case "name":
      return (x, y) => collator.compare(x.name, y.name);
    case "name-desc":
      return (x, y) => collator.compare(y.name, x.name);
    case "machines":
      return (x, y) => machinesOf(y).length - machinesOf(x).length;
    case "machines-few":
      return (x, y) => machinesOf(x).length - machinesOf(y).length;
    case "active":
      return (x, y) => activeAt(y) - activeAt(x);
    case "run":
      return (x, y) => (y.latest?.created_at ?? 0) - (x.latest?.created_at ?? 0);
    case "polled":
      return (x, y) => (y.built?.last_polled_at ?? 0) - (x.built?.last_polled_at ?? 0);
    case "added":
      return (x, y) => Math.max(y.built?.created_at ?? 0, y.synced?.created ?? 0) - Math.max(x.built?.created_at ?? 0, x.synced?.created ?? 0);
  }
}

export function sortEntries(list: Entry[], sort: SortId): Entry[] {
  const by = sortBy(SORTS.some((s) => s.value === sort) ? sort : "name");
  return [...list].sort((x, y) => by(x, y) || collator.compare(x.name, y.name) || x.key.localeCompare(y.key));
}

/* ---------------------------------------------------------------- folders on other machines */

/** `path` as `~/…` when it is under `home` (`/` separators, as the server takes them on any system). */
export function tildeOf(path: string, home: string | null | undefined): string | null {
  if (!home) return null;
  const norm = (s: string) => s.replace(/\\/g, "/").replace(/\/+$/, "");
  const p = norm(path);
  const h = norm(home);
  const same = /^[a-z]:/i.test(h) ? p.toLowerCase().startsWith(`${h.toLowerCase()}/`) : p.startsWith(`${h}/`);
  return same ? `~/${p.slice(h.length + 1)}` : null;
}

/** The last part of a repository's name, as a folder is named. */
export const folderName = (name: string) => name.split(/[/:\\]/).filter(Boolean).pop() ?? name;

/**
 * Where a repository goes on another machine, unless told otherwise: where the source is relative
 * to its home (`~/Projects/kiln` stays `~/Projects/kiln`), else where it is elsewhere, else
 * `~/Projects/<name>`.
 */
export function defaultPath(e: Entry, machines: Map<string, PlaceMachine>, source?: Place | null): string {
  const from = [source, ...e.places].filter((p): p is Place => Boolean(p));
  for (const p of from) {
    const t = tildeOf(p.path, machines.get(p.machine)?.home);
    if (t) return t;
  }
  return `~/Projects/${folderName(e.name)}`;
}

/** What `~/…` comes to on a machine (as the server resolves it), to show beside the field. */
export function resolvedOn(path: string, m: PlaceMachine | undefined): string | null {
  const p = path.trim();
  if (!p.startsWith("~")) return null;
  if (!m?.home) return null;
  const win = m.home.includes("\\");
  const rest = p === "~" ? "" : p.replace(/^~[\\/]/, "");
  const sep = win ? "\\" : "/";
  return rest ? `${m.home.replace(/[\\/]+$/, "")}${sep}${win ? rest.replace(/\//g, "\\") : rest}` : m.home;
}

/** A remote people can open in a browser, from its git form. */
export function browseUrl(origin: string | null): string | null {
  if (!origin) return null;
  const o = origin.trim().replace(/\.git$/, "");
  if (/^https?:\/\//i.test(o)) return o;
  const scp = /^(?:[^@/]+@)?([^:/]+):(.+)$/.exec(o);
  if (scp) return `https://${scp[1]}/${scp[2]}`;
  const ssh = /^ssh:\/\/(?:[^@/]+@)?([^/:]+)(?::\d+)?\/(.+)$/i.exec(o);
  return ssh ? `https://${ssh[1]}/${ssh[2]}` : null;
}
