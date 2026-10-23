/**
 * Continuous profiles (docs/observe.md §8.11), Datadog's Continuous Profiler: what tracers' profilers upload every minute (CPU,
 * allocations, the heap, locks, goroutines), through the runner on their machine or straight to the server. A type summed into a
 * flame graph over the uploads a search of their tags chooses, and its functions by self; each upload listed, its own graph a
 * click away, its profiles downloadable as pprof.
 */
import * as React from "react";
import { observeApi, type FlameComparison, type FlameGraph, type ProfileFacet, type ProfileType, type ProfileUpload } from "@/api/observe";
import { TableHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { PageIntro, SelectField } from "@/components/ui/misc";
import { Segmented } from "@/components/ui/tabs";
import { useRes, type Res } from "@/lib/data";
import { formatBytes } from "@/lib/format";
import { useCanvas } from "./heatmap";
import { ObserveNav, RANGES, RangePicker, compact, formatSeconds, pollFor, useParam, useParams, useRange, type Range } from "./parts";
import { PIE_HUES } from "./widgets";
import { SERIES } from "@/components/charts/types";

const span = (range: Range) => {
  const to = Date.now();
  return { from: to - RANGES[range], to };
};

const typesRes = (range: Range, q: string): Res<ProfileType[]> => ({
  key: `observe/profiles/types/${range}/${q}`,
  fetch: (s) => observeApi.profileTypes({ ...span(range), query: q || undefined }, s),
  opts: { pollMs: pollFor(range) },
});

const facetsRes = (range: Range, q: string): Res<{ facets: ProfileFacet[]; matched: number }> => ({
  key: `observe/profiles/facets/${range}/${q}`,
  fetch: (s) => observeApi.profileFacets({ ...span(range), query: q || undefined }, s),
  opts: { pollMs: pollFor(range) },
});

const uploadsRes = (range: Range, q: string): Res<{ uploads: ProfileUpload[]; matched: number }> => ({
  key: `observe/profiles/uploads/${range}/${q}`,
  fetch: (s) => observeApi.profiles({ ...span(range), query: q || undefined, limit: 200 }, s),
  opts: { pollMs: pollFor(range) },
});

/** A flame graph is a summary of its time, read once: not read again as it shows. */
const flameRes = (range: Range, q: string, t: ProfileType, id: string): Res<FlameGraph> => ({
  key: `observe/profiles/flame/${range}/${q}/${t.profile}/${t.type}/${id}`,
  fetch: (s) => observeApi.flame({ ...span(range), query: q || undefined, profile: t.profile, type: t.type, id: id || undefined }, s),
});

const uploadRes = (id: string): Res<ProfileUpload> => ({ key: `observe/profiles/upload/${id}`, fetch: (s) => observeApi.profileUpload(id, s) });

/** What a graph is compared with: the period before, the same time a day or a week before, or another search at the same time. */
export type Against = "prev" | "day" | "week" | "search";
const AGAINST: { value: Against | ""; label: string }[] = [
  { value: "", label: "Nothing" },
  { value: "prev", label: "The period before" },
  { value: "day", label: "A day before" },
  { value: "week", label: "A week before" },
  { value: "search", label: "Another search" },
];
const isAgainst = (v: string): v is Against => v === "prev" || v === "day" || v === "week" || v === "search";

/** The other side's time: the range just before, or the range a day or a week before; the same for another search. */
export function baseOf(from: number, to: number, against: Against): { base_from: number; base_to: number } {
  const back = against === "prev" ? to - from : against === "day" ? 86_400_000 : against === "week" ? 7 * 86_400_000 : 0;
  return { base_from: from - back, base_to: to - back };
}

/** A comparison is a summary of its times, read once. */
const compareRes = (range: Range, q: string, t: ProfileType, id: string, against: Against, bq: string): Res<FlameComparison> => ({
  key: `observe/profiles/compare/${range}/${q}/${t.profile}/${t.type}/${id}/${against}/${bq}`,
  fetch: (s) => {
    const { from, to } = span(range);
    return observeApi.compareFlames(
      { from, to, query: q || undefined, profile: t.profile, type: t.type, id: id || undefined, ...baseOf(from, to, against), base_query: against === "search" ? bq : undefined },
      s,
    );
  },
});

/** Types' names as Datadog's profiler pages say them, by the type pprof gives (Go's, Python's, Node's, .NET's, Ruby's). */
const LABELS: Record<string, string> = {
  cpu: "CPU time",
  "cpu-time": "CPU time",
  wall: "Wall time",
  "wall-time": "Wall time",
  alloc_space: "Allocated memory",
  "alloc-space": "Allocated memory",
  alloc_objects: "Allocations",
  "alloc-samples": "Allocations",
  inuse_space: "Heap live size",
  "heap-space": "Heap live size",
  "heap-live-size": "Heap live size",
  inuse_objects: "Heap live objects",
  "heap-live-samples": "Heap live objects",
  goroutine: "Goroutines",
  "lock-acquire-wait": "Lock wait",
  "lock-release-hold": "Lock held",
  "exception-samples": "Exceptions",
};
/** Types first in the list, and the one shown unless another is asked for: CPU time's. */
const FIRST = ["cpu", "cpu-time", "wall-time", "wall", "alloc_space", "alloc-space"];

const keyOf = (t: ProfileType) => `${t.profile}:${t.type}`;

/** A type's name: Go's mutex and block profiles share their types' (`contentions`, `delay`), told apart by their profile. */
export function typeLabel(t: ProfileType): string {
  if (t.type === "contentions" || t.type === "delay") {
    const of = t.profile.includes("mutex") ? "Mutex" : t.profile.includes("block") ? "Block" : t.profile;
    return `${of} ${t.type === "delay" ? "wait" : "contentions"}`;
  }
  return LABELS[t.type] ?? `${t.type} (${t.profile})`;
}

/** A value in its pprof unit: time, bytes, or a count. */
export function valueText(v: number, unit: string): string {
  if (unit === "nanoseconds") return formatSeconds(v / 1e9);
  if (unit === "bytes") return formatBytes(v);
  return compact(v);
}

/** The types to choose from: CPU time's first, then by name; a CPU profile's sample count left out beside its time. */
function ordered(types: ProfileType[]): ProfileType[] {
  const rank = (t: ProfileType) => {
    const i = FIRST.indexOf(t.type);
    return i === -1 ? FIRST.length : i;
  };
  return types
    .filter((t) => !(t.type === "samples" && types.some((o) => o.profile === t.profile && o.type !== "samples")))
    .sort((a, b) => rank(a) - rank(b) || typeLabel(a).localeCompare(typeLabel(b)));
}

export default function Profiles() {
  const [range, setRange] = useRange("1h");
  const [q] = useParam("q", "");
  const [type] = useParam("type", "");
  const [id] = useParam("id", "");
  const [view] = useParam("view", "flame");
  const [cmp] = useParam("cmp", "");
  const [bq] = useParam("bq", "");
  const set = useParams();
  const shown = view === "uploads" ? "uploads" : "flame";
  return (
    <div className="page">
      <title>Profiles · Observe · kiln</title>
      <PageIntro
        index={8}
        label="Observe"
        title="Profiles"
        lead="What your services spend their CPU, memory and time waiting on, function by function: Datadog's tracers' profilers upload every minute to the runner on their machine (its Agent's port, 8126), or straight to the server."
      />
      <ObserveNav />
      <div className="mb-4 flex flex-wrap items-end gap-3">
        <Segmented
          label="View"
          value={shown}
          onChange={(v) => set({ view: v === "flame" ? null : v })}
          options={[
            { value: "flame", label: "Flame graph" },
            { value: "uploads", label: "Uploads" },
          ]}
        />
        <RangePicker value={range} onChange={setRange} />
        <Search key={q} q={q} onQ={(v) => set({ q: v || null, id: null })} />
      </div>
      <Loadable key={`${range}/${q}`} label="the profiles' tags" fallback={<div className="mb-4 h-9" />}>
        <Facets range={range} q={q} onQ={(v) => set({ q: v || null, id: null })} />
      </Loadable>
      {id && (
        <Loadable key={id} label="the upload">
          <OneUpload id={id} onAll={() => set({ id: null })} />
        </Loadable>
      )}
      {shown === "uploads" ? (
        <Loadable key={`${range}/${q}`} label="the uploads" fallback={<TableHold head={<UploadsHead />} rows={6} />}>
          <Uploads range={range} q={q} onPick={(u) => set({ id: u, view: null })} />
        </Loadable>
      ) : (
        <Loadable key={`${range}/${q}/${id}`} label="the profiles' types">
          <FlameSection
            range={range}
            q={q}
            type={type}
            id={id}
            against={isAgainst(cmp) ? cmp : null}
            bq={bq}
            onType={(t) => set({ type: t })}
            onAgainst={(a, b) => set({ cmp: a || null, bq: a === "search" ? b || null : null })}
          />
        </Loadable>
      )}
    </div>
  );
}

function Search({ q, onQ }: { q: string; onQ: (q: string) => void }) {
  const [text, setText] = React.useState(q);
  return (
    <form
      className="field w-full sm:ml-auto sm:w-96"
      onSubmit={(e) => {
        e.preventDefault();
        onQ(text.trim());
      }}
    >
      <input aria-label="Search the uploads' tags" type="search" value={text} onChange={(e) => setText(e.target.value)} placeholder="service:checkout env:prod -host:web-2" spellCheck={false} />
    </form>
  );
}

/** `q` with its `key:` term (a lone one, as a facet sets it) made `key:value`, or taken out. */
export function withTerm(q: string, key: string, value: string | null): string {
  const terms = q.split(/\s+/).filter((t) => t && !t.startsWith(`${key}:`));
  if (value !== null) terms.push(`${key}:${/[\s"():]/.test(value) ? JSON.stringify(value) : value}`);
  return terms.join(" ");
}

/** The current term of `key` in `q`, as a facet set it. */
const termOf = (q: string, key: string) =>
  q
    .split(/\s+/)
    .find((t) => t.startsWith(`${key}:`))
    ?.slice(key.length + 1) ?? "";

/** The unified service tags' values among the uploads (service, env, version, host): one chosen narrows the search to it. */
function Facets({ range, q, onQ }: { range: Range; q: string; onQ: (q: string) => void }) {
  const { facets } = useRes(facetsRes(range, q)).data;
  const shown = facets.filter((f) => ["service", "env", "version", "host"].includes(f.key));
  if (shown.length === 0) return null;
  return (
    <div className="mb-4 flex flex-wrap gap-3">
      {shown.map((f) => {
        const now = termOf(q, f.key);
        const values = f.values.map((v) => ({ value: v.value, label: `${v.value} · ${v.uploads.toLocaleString()}` }));
        if (now && !f.values.some((v) => v.value === now)) values.unshift({ value: now, label: now });
        return (
          <SelectField
            key={f.key}
            label={f.key}
            value={now}
            onChange={(v) => onQ(withTerm(q, f.key, v || null))}
            options={[{ value: "", label: `Any ${f.key}` }, ...values]}
            className="min-w-[10rem]"
          />
        );
      })}
    </div>
  );
}

/** The upload a graph is of: when, of what, its profiles to download; back to every upload. */
function OneUpload({ id, onAll }: { id: string; onAll: () => void }) {
  const u = useRes(uploadRes(id)).data;
  const tag = (k: string) => u.tags.find((t) => t.startsWith(`${k}:`))?.slice(k.length + 1);
  return (
    <div className="mb-4 flex flex-wrap items-center gap-x-4 gap-y-2 rounded-xl border border-white/10 bg-white/[0.02] px-4 py-3 text-[13px]">
      <span>
        One upload: <span className="font-mono">{tag("service") ?? "—"}</span>
        {tag("host") ? <span className="text-[#8c8f98]"> on {tag("host")}</span> : null}
        <span className="text-[#8c8f98]"> · {new Date(u.start_ms).toLocaleString()}</span>
      </span>
      <span className="flex flex-wrap gap-2 text-[12px]">
        {u.profiles.map((p) => (
          <a key={p.name} href={observeApi.profileDownload(id, p.name)} download={p.name} className="font-mono text-[#a6c4ed] hover:text-white">
            {p.name}
          </a>
        ))}
      </span>
      <button type="button" className="btn btn-ghost ml-auto text-[12.5px]" onClick={onAll}>
        Every upload
      </button>
    </div>
  );
}

function FlameSection({
  range,
  q,
  type,
  id,
  against,
  bq,
  onType,
  onAgainst,
}: {
  range: Range;
  q: string;
  type: string;
  id: string;
  against: Against | null;
  bq: string;
  onType: (t: string) => void;
  onAgainst: (a: Against | "", bq: string) => void;
}) {
  const all = useRes(typesRes(range, q)).data;
  const types = ordered(all);
  if (types.length === 0)
    return (
      <p className="empty-line py-12 text-center">
        {q
          ? "No upload matches in this time."
          : "No profile in this time. Turn a tracer's profiler on (DD_PROFILING_ENABLED=true) with the runner on its machine as its Agent, or point its uploads at the server."}
      </p>
    );
  const chosen = types.find((t) => keyOf(t) === type) ?? types[0];
  if (!chosen) return null;
  return (
    <>
      <div className="mb-3 flex flex-wrap items-end gap-3">
        <SelectField
          label="Type"
          value={keyOf(chosen)}
          onChange={onType}
          options={types.map((t) => ({ value: keyOf(t), label: `${typeLabel(t)} · ${t.uploads.toLocaleString()}` }))}
          className="min-w-[14rem]"
        />
        <SelectField label="Compare with" value={against ?? ""} onChange={(v) => onAgainst(isAgainst(v) ? v : "", bq || q)} options={AGAINST} className="min-w-[12rem]" />
        {against === "search" && <BaseSearch key={bq} bq={bq} onBq={(v) => onAgainst("search", v)} />}
      </div>
      {against ? (
        <Loadable key={`${keyOf(chosen)}/${against}/${bq}`} label="the comparison">
          <Comparison range={range} q={q} t={chosen} id={id} against={against} bq={bq} />
        </Loadable>
      ) : (
        <Loadable key={keyOf(chosen)} label="the flame graph">
          <Flame range={range} q={q} t={chosen} id={id} />
        </Loadable>
      )}
    </>
  );
}

function BaseSearch({ bq, onBq }: { bq: string; onBq: (bq: string) => void }) {
  const [text, setText] = React.useState(bq);
  return (
    <form
      className="field w-full sm:w-80"
      onSubmit={(e) => {
        e.preventDefault();
        onBq(text.trim());
      }}
    >
      <input aria-label="The search compared with" type="search" value={text} onChange={(e) => setText(e.target.value)} placeholder="service:checkout version:1.4.2" spellCheck={false} />
    </form>
  );
}

/** What a comparison's other side is, in words. */
const againstText = (a: Against, bq: string) => (a === "prev" ? "the period before" : a === "day" ? "a day before" : a === "week" ? "a week before" : `“${bq || "*"}”`);

/** A type compared path by path with another time's or search's: drawn as now, each frame coloured by how its share of the whole changed. */
function Comparison({ range, q, t, id, against, bq }: { range: Range; q: string; t: ProfileType; id: string; against: Against; bq: string }) {
  const c = useRes(compareRes(range, q, t, id, against, bq)).data;
  const [find, setFind] = React.useState("");
  const g: FlameGraph = React.useMemo(
    () => ({
      profile: c.profile,
      type: c.type,
      unit: c.unit,
      averaged: c.averaged,
      matched: c.after.matched,
      read: c.after.read,
      failed: c.after.failed,
      samples: c.after.samples,
      total: c.after.total,
      names: c.names,
      files: c.files,
      nodes: c.nodes.map((n): [number, number, number, number] => [n[0], n[1], n[2], n[3]]),
      top: [],
    }),
    [c],
  );
  const before = React.useMemo(() => c.nodes.map((n) => n[4]), [c]);
  if (c.after.read === 0 || c.after.total === 0) return <p className="empty-line py-12 text-center">No upload holds this type in this time.</p>;
  if (c.before.read === 0 || c.before.total === 0) return <p className="empty-line py-12 text-center">No upload holds this type in what it is compared with ({againstText(against, bq)}).</p>;
  return (
    <>
      <p className="mb-2 text-[12.5px] text-[#8c8f98]">
        {typeLabel(t)}: {valueText(c.after.total, c.unit)} in {c.after.read.toLocaleString()} {c.after.read === 1 ? "upload" : "uploads"}, against {valueText(c.before.total, c.unit)} in{" "}
        {c.before.read.toLocaleString()} {c.before.read === 1 ? "upload" : "uploads"} {against === "search" ? `of ${againstText(against, bq)}` : againstText(against, bq)}
        {c.averaged ? " (each side's average over time)" : ""}. Frames are coloured by how their share of the whole changed.
      </p>
      <FlameChart g={g} find={find} diff={{ before, whole: [c.before.total, c.after.total] }} />
      <Changes c={c} find={find} onFind={setFind} />
    </>
  );
}

/** A comparison's functions by how much their self's share of each whole changed: before, after, the change in points. */
function Changes({ c, find, onFind }: { c: FlameComparison; find: string; onFind: (f: string) => void }) {
  const rows = c.top.slice(0, 50);
  const share = (v: number, whole: number) => (whole > 0 ? (100 * v) / whole : 0);
  return (
    <div className="mt-6">
      <div className="mb-2 flex flex-wrap items-end gap-3">
        <h3 className="text-[14px] text-[#e4e5e7]">Functions by change</h3>
        <form className="field ml-auto w-full sm:w-72" onSubmit={(e) => e.preventDefault()}>
          <input aria-label="Find a function in the graph" type="search" value={find} onChange={(e) => onFind(e.target.value)} placeholder="Find a function" spellCheck={false} />
        </form>
      </div>
      <div className="table-wrap">
        <table className="data-table">
          <thead>
            <tr>
              <th>Function</th>
              <th className="hidden text-right sm:table-cell">Self before</th>
              <th className="hidden text-right sm:table-cell">Self now</th>
              <th className="text-right">Change</th>
            </tr>
          </thead>
          <tbody>
            {rows.map(([f, after, before]) => {
              const d = share(after, c.after.total) - share(before, c.before.total);
              return (
                <tr key={f}>
                  <td className="truncate-cell">
                    <button type="button" className="text-left font-mono text-[12.5px] text-[#e4e5e7] hover:text-white" onClick={() => onFind(find === c.names[f] ? "" : (c.names[f] ?? ""))}>
                      {shortName(c.names[f] ?? "")}
                    </button>
                    <p className="truncate text-[11px] text-[#8c8f98]">{c.files[f] || "—"}</p>
                  </td>
                  <td className="hidden whitespace-nowrap text-right font-mono text-[12px] sm:table-cell">
                    {valueText(before, c.unit)} <span className="text-[#8c8f98]">· {pct(before, c.before.total)}</span>
                  </td>
                  <td className="hidden whitespace-nowrap text-right font-mono text-[12px] sm:table-cell">
                    {valueText(after, c.unit)} <span className="text-[#8c8f98]">· {pct(after, c.after.total)}</span>
                  </td>
                  <td className="whitespace-nowrap text-right font-mono text-[12px]">
                    {d > 0 ? "+" : d < 0 ? "−" : ""}
                    {Math.abs(d) < 0.05 && d !== 0 ? "<0.1" : Math.abs(d).toFixed(1)} pts
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
    </div>
  );
}

/** Where a function comes from: your code, a library, or its language's runtime and standard library (Go's, Python's, Node's, the JVM's, .NET's); unknown otherwise. */
type Kind = "own" | "library" | "runtime" | "other";

export function kindOf(name: string, file: string): Kind {
  // Go: `main`'s, a module's (its path's first element a domain: a library where it was built from the module cache or vendored),
  // or the standard library's and the runtime's (no domain).
  if (/\.(go|s)$/.test(file)) {
    if (name.startsWith("main.")) return "own";
    const first = name.includes("/") ? name.slice(0, name.indexOf("/")) : name.slice(0, Math.max(0, name.indexOf(".")));
    if (first.includes(".")) return /\/pkg\/mod\/|\/vendor\//.test(file) ? "library" : "own";
    return "runtime";
  }
  if (/site-packages|dist-packages|node_modules|\/gems\/|\.m2\/|\/nuget\//.test(file)) return "library";
  if (/\/lib\/python3|^<frozen|^node:|^internal\/|\/jdk\/|^java\.|^jdk\.|^System\.|\/usr\/lib\/ruby/.test(file || name)) return "runtime";
  return file ? "own" : "other";
}

/** Frames' hues by where their function comes from: kiln's three validated for any two to touch, and grey. */
const KIND_HUE: Record<Kind, string> = { own: PIE_HUES[1] ?? "#416aa2", library: PIE_HUES[0] ?? "#9f71dc", runtime: PIE_HUES[2] ?? "#994e5d", other: SERIES.grey };
const KIND_LABEL: Record<Kind, string> = { own: "Your code", library: "Libraries", runtime: "Runtime", other: "Other" };

/** Text over a fill: whichever of the two inks reads better on it. */
function inkOn(hex: string): string {
  const n = parseInt(hex.slice(1, 7), 16);
  const lin = (c: number) => {
    const v = c / 255;
    return v <= 0.04045 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4;
  };
  const l = 0.2126 * lin((n >> 16) & 255) + 0.7152 * lin((n >> 8) & 255) + 0.0722 * lin(n & 255);
  return 1.05 / (l + 0.05) >= (l + 0.05) / 0.05 ? "#f2f2f3" : "#0b0c0f";
}

/** A function's name as a frame shows it: a Go name's package path before its last element left out. */
export const shortName = (name: string) => name.replace(/^(?:[\w.-]+\/)+(?=[\w.-]+\.)/, "");

const ROW = 18;

interface Layout {
  /** Each node's left (in its unit, from the root's), depth, and the nodes at each depth left to right. */
  x: Float64Array;
  depth: Int32Array;
  byDepth: number[][];
  kinds: Kind[];
}

/** Each node placed: its children side by side under it, in the order they come (by name), each as wide as its total. */
function layout(g: FlameGraph): Layout {
  const n = g.nodes.length;
  const x = new Float64Array(n);
  const depth = new Int32Array(n);
  const next = new Float64Array(n);
  const byDepth: number[][] = [[0]];
  for (let i = 1; i < n; i++) {
    const node = g.nodes[i];
    if (!node) continue;
    const p = node[1];
    x[i] = next[p] ?? 0;
    next[p] = (next[p] ?? 0) + node[2];
    next[i] = x[i] ?? 0;
    const d = (depth[p] ?? 0) + 1;
    depth[i] = d;
    (byDepth[d] ??= []).push(i);
  }
  const kinds = g.names.map((name, f) => kindOf(name, g.files[f] ?? ""));
  return { x, depth, byDepth, kinds };
}

/** The node under a point: at its depth, the one whose span holds it (each depth's nodes left to right). */
function nodeAt(g: FlameGraph, l: Layout, focus: number, w: number, px: number, py: number): number | null {
  const d = Math.floor(py / ROW);
  const row = l.byDepth[d];
  if (!row) return null;
  const f = g.nodes[focus];
  if (!f) return null;
  const x0 = l.x[focus] ?? 0;
  const v = x0 + (px / w) * Math.max(1, f[2]);
  // Above the focus: its ancestors, each drawn across.
  if (d < (l.depth[focus] ?? 0)) {
    let a: number = focus;
    while (a !== 0 && (l.depth[a] ?? 0) > d) a = g.nodes[a]?.[1] ?? 0;
    return a;
  }
  let lo = 0;
  let hi = row.length - 1;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    const i = row[mid] ?? 0;
    const left = l.x[i] ?? 0;
    const node = g.nodes[i];
    if (!node) return null;
    if (v < left) hi = mid - 1;
    else if (v >= left + node[2]) lo = mid + 1;
    else return v >= x0 && v < x0 + f[2] ? i : null;
  }
  return null;
}

function Flame({ range, q, t, id }: { range: Range; q: string; t: ProfileType; id: string }) {
  const g = useRes(flameRes(range, q, t, id)).data;
  const [find, setFind] = React.useState("");
  if (g.read === 0 || g.total === 0)
    return <p className="empty-line py-12 text-center">{g.failed > 0 ? `None of the ${g.failed.toLocaleString()} uploads holding it could be read.` : "No upload holds this type in this time."}</p>;
  return (
    <>
      <p className="mb-2 text-[12.5px] text-[#8c8f98]">
        {typeLabel(t)}: {valueText(g.total, g.unit)} {g.averaged ? "on average over" : "in"} {g.read.toLocaleString()} {g.read === 1 ? "upload" : "uploads"}
        {g.matched > g.read ? ` (of ${g.matched.toLocaleString()}, read evenly across the time)` : ""}
        {g.failed > 0 ? `; ${g.failed.toLocaleString()} could not be read` : ""}.
      </p>
      <FlameChart g={g} find={find} />
      <Top g={g} find={find} onFind={setFind} />
    </>
  );
}

function ChangeLegend() {
  return (
    <ul className="mb-2 flex flex-wrap gap-4 text-[12px] text-[#b4b7bf]" aria-label="Frames' colours">
      {(
        [
          [PIE_HUES[2] ?? "#994e5d", "A larger share now"],
          [PIE_HUES[1] ?? "#416aa2", "A smaller share now"],
          [NEUTRAL, "The same share"],
        ] as const
      ).map(([hue, label]) => (
        <li key={label} className="flex items-center gap-1.5">
          <span className="inline-block h-2.5 w-2.5 rounded-sm" style={{ background: hue }} />
          {label}
        </li>
      ))}
    </ul>
  );
}

function Legend({ kinds }: { kinds: Set<Kind> }) {
  return (
    <ul className="mb-2 flex flex-wrap gap-4 text-[12px] text-[#b4b7bf]" aria-label="Frames' colours">
      {(Object.keys(KIND_LABEL) as Kind[])
        .filter((k) => kinds.has(k))
        .map((k) => (
          <li key={k} className="flex items-center gap-1.5">
            <span className="inline-block h-2.5 w-2.5 rounded-sm" style={{ background: KIND_HUE[k] }} />
            {KIND_LABEL[k]}
          </li>
        ))}
    </ul>
  );
}

/** A change's colour: none grey, a share grown towards rose, one shrunk towards blue (kiln's pair, held apart for any two to touch), as far as the most changed. */
const NEUTRAL = "#383835";
function mix(a: string, b: string, t: number) {
  const pa = parseInt(a.slice(1, 7), 16);
  const pb = parseInt(b.slice(1, 7), 16);
  const ch = (shift: number) => Math.round(((pa >> shift) & 255) * (1 - t) + ((pb >> shift) & 255) * t);
  return `#${((ch(16) << 16) | (ch(8) << 8) | ch(0)).toString(16).padStart(6, "0")}`;
}

/**
 * A flame graph, root at the top (Datadog's way up): each frame a function, under its caller, as wide as its total. A frame
 * clicked fills the width (its callers above it, across); the root clicked, back. Frames a search finds stay, the rest dim.
 * Compared (`diff`), each frame coloured by how its share of the whole changed.
 */
function FlameChart({ g, find, diff }: { g: FlameGraph; find: string; diff?: { before: number[]; whole: [number, number] } }) {
  const l = React.useMemo(() => layout(g), [g]);
  const [focus, setFocus] = React.useState(0);
  const [hover, setHover] = React.useState<{ i: number; x: number; y: number; w: number } | null>(null);
  const found = React.useMemo(() => {
    const f = find.trim().toLowerCase();
    return f ? new Set(g.names.flatMap((n, i) => (n.toLowerCase().includes(f) ? [i] : []))) : null;
  }, [g, find]);
  const kinds = React.useMemo(() => new Set(g.nodes.slice(1).map((n) => l.kinds[n[0]] ?? "other")), [g, l]);
  // Each frame's change of its share of the whole, and the most any changed.
  const change = React.useMemo(() => {
    if (!diff) return null;
    const [wb, wa] = diff.whole;
    const d = g.nodes.map((n, i) => (wa > 0 ? n[2] / wa : 0) - (wb > 0 ? (diff.before[i] ?? 0) / wb : 0));
    const most = d.reduce((m, v, i) => (i === 0 ? m : Math.max(m, Math.abs(v))), 0);
    return { d, most };
  }, [g, diff]);
  const focusNode = g.nodes[focus];
  const fd = l.depth[focus] ?? 0;
  // The rows drawn: the focus's callers, then its subtree's deepest.
  const rows = React.useMemo(() => {
    let deepest = fd;
    const x0 = l.x[focus] ?? 0;
    const w = focusNode?.[2] ?? 0;
    for (let i = focus; i < g.nodes.length; i++) {
      const xi = l.x[i] ?? 0;
      if ((l.depth[i] ?? 0) >= fd && xi >= x0 && xi < x0 + w) deepest = Math.max(deepest, l.depth[i] ?? 0);
    }
    return deepest + 1;
  }, [g, l, focus, fd, focusNode]);
  const height = rows * ROW;
  const paint = React.useCallback(
    (ctx: CanvasRenderingContext2D, w: number) => {
      if (!focusNode) return;
      const x0 = l.x[focus] ?? 0;
      const total = Math.max(1, focusNode[2]);
      const scale = w / total;
      ctx.font = '11px "Geist Mono Variable", ui-monospace, monospace';
      ctx.textBaseline = "middle";
      const frame = (i: number, left: number, width: number, faded: boolean) => {
        const node = g.nodes[i];
        if (!node) return;
        const d = l.depth[i] ?? 0;
        const fn = node[0];
        const delta = change?.d[i] ?? 0;
        const hue = change
          ? fn < 0 || change.most === 0
            ? NEUTRAL
            : mix(NEUTRAL, delta > 0 ? (PIE_HUES[2] ?? "#994e5d") : (PIE_HUES[1] ?? "#416aa2"), Math.min(1, Math.abs(delta) / change.most))
          : fn < 0
            ? SERIES.grey
            : KIND_HUE[l.kinds[fn] ?? "other"];
        const lit = !found || (fn >= 0 && found.has(fn));
        ctx.globalAlpha = faded ? 0.45 : lit ? 1 : 0.22;
        ctx.fillStyle = hue;
        // A surface gap between frames, and below each row.
        ctx.fillRect(left, d * ROW, Math.max(0.5, width - (width > 3 ? 1 : 0)), ROW - 1);
        if (width > 34) {
          const label = fn < 0 ? `all · ${valueText(node[2], g.unit)}` : shortName(g.names[fn] ?? "");
          const room = Math.floor((width - 8) / 6.7);
          ctx.fillStyle = inkOn(hue);
          ctx.fillText(label.length > room ? `${label.slice(0, Math.max(0, room - 1))}…` : label, left + 4, d * ROW + ROW / 2);
        }
      };
      // The focus's callers, across.
      let a = focus;
      while (a !== 0) {
        a = g.nodes[a]?.[1] ?? 0;
        frame(a, 0, w, true);
      }
      for (let i = focus; i < g.nodes.length; i++) {
        const node = g.nodes[i];
        if (!node) continue;
        const xi = l.x[i] ?? 0;
        if ((l.depth[i] ?? 0) < fd || xi < x0 || xi >= x0 + total) continue;
        const width = node[2] * scale;
        if (width < 0.35) continue;
        frame(i, (xi - x0) * scale, width, false);
      }
      ctx.globalAlpha = 1;
    },
    [g, l, focus, fd, focusNode, found, change],
  );
  const ref = useCanvas(paint, height);
  const at = (e: React.MouseEvent<HTMLCanvasElement>) => {
    const box = e.currentTarget.getBoundingClientRect();
    const x = e.clientX - box.left;
    const y = e.clientY - box.top;
    const i = nodeAt(g, l, focus, box.width, x, y);
    return i === null ? null : { i, x, y, w: box.width };
  };
  const shown = hover ? g.nodes[hover.i] : undefined;
  const fn = shown ? shown[0] : -1;
  return (
    <div>
      <div className="mb-2 flex flex-wrap items-center gap-3">
        {change ? <ChangeLegend /> : <Legend kinds={kinds} />}
        {focus !== 0 && (
          <button type="button" className="btn btn-ghost ml-auto text-[12px]" onClick={() => setFocus(0)}>
            Whole graph
          </button>
        )}
      </div>
      <div className="relative overflow-hidden rounded-lg border border-white/10 bg-[#0a0c0f]">
        <canvas
          ref={ref}
          style={{ width: "100%", height, display: "block", cursor: "pointer" }}
          onPointerMove={(e) => setHover(at(e))}
          onPointerDown={(e) => setHover(at(e))}
          onPointerLeave={(e) => e.pointerType === "mouse" && setHover(null)}
          onClick={(e) => {
            const h = at(e);
            if (h) setFocus(h.i === focus ? (g.nodes[focus]?.[1] ?? 0) : h.i);
          }}
          role="img"
          aria-label={`A flame graph of ${g.nodes.length.toLocaleString()} frames, ${valueText(g.total, g.unit)} in all; its functions by self are in the table below.`}
        />
        {hover && shown && (
          <div
            className="obs-tooltip pointer-events-none absolute z-10 max-w-[28rem] rounded border border-[#2a2d34] bg-[#121317] px-2 py-1 font-mono text-[11px] text-[#e4e5e7] shadow"
            style={{ left: Math.min(hover.x + 12, Math.max(0, hover.w - 300)), top: hover.y + 14 }}
          >
            <div className="break-all">{fn < 0 ? "All" : g.names[fn]}</div>
            {fn >= 0 && g.files[fn] ? <div className="break-all text-[#8c8f98]">{g.files[fn]}</div> : null}
            <div>
              {valueText(shown[2], g.unit)} · {pct(shown[2], g.total)} of all
              {shown[3] > 0 ? <span className="text-[#8c8f98]"> · self {valueText(shown[3], g.unit)}</span> : null}
            </div>
            {diff ? (
              <div className="text-[#8c8f98]">
                before {valueText(diff.before[hover.i] ?? 0, g.unit)} · {pct(diff.before[hover.i] ?? 0, diff.whole[0])} of all
              </div>
            ) : null}
          </div>
        )}
      </div>
    </div>
  );
}

const pct = (v: number, of: number) => {
  if (of <= 0) return "—";
  const p = (100 * v) / of;
  return p > 0 && p < 0.1 ? "<0.1%" : `${p >= 10 ? p.toFixed(0) : p.toFixed(1)}%`;
};

/** The functions by self (a recursive one's total counted once): the first 50; one clicked found in the graph. */
function Top({ g, find, onFind }: { g: FlameGraph; find: string; onFind: (f: string) => void }) {
  const rows = g.top.slice(0, 50);
  return (
    <div className="mt-6">
      <div className="mb-2 flex flex-wrap items-end gap-3">
        <h3 className="text-[14px] text-[#e4e5e7]">Functions by self</h3>
        <form className="field ml-auto w-full sm:w-72" onSubmit={(e) => e.preventDefault()}>
          <input aria-label="Find a function in the graph" type="search" value={find} onChange={(e) => onFind(e.target.value)} placeholder="Find a function" spellCheck={false} />
        </form>
      </div>
      <div className="table-wrap">
        <table className="data-table">
          <thead>
            <tr>
              <th>Function</th>
              <th className="text-right">Self</th>
              <th className="hidden text-right sm:table-cell">Total</th>
            </tr>
          </thead>
          <tbody>
            {rows.map(([f, own, total]) => (
              <tr key={f}>
                <td className="truncate-cell">
                  <button type="button" className="text-left font-mono text-[12.5px] text-[#e4e5e7] hover:text-white" onClick={() => onFind(find === g.names[f] ? "" : (g.names[f] ?? ""))}>
                    {shortName(g.names[f] ?? "")}
                  </button>
                  <p className="truncate text-[11px] text-[#8c8f98]">{g.files[f] || "—"}</p>
                </td>
                <td className="whitespace-nowrap text-right font-mono text-[12px]">
                  {valueText(own, g.unit)} <span className="text-[#8c8f98]">· {pct(own, g.total)}</span>
                </td>
                <td className="hidden whitespace-nowrap text-right font-mono text-[12px] sm:table-cell">
                  {valueText(total, g.unit)} <span className="text-[#8c8f98]">· {pct(total, g.total)}</span>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </div>
  );
}

function UploadsHead() {
  return (
    <tr>
      <th>Upload</th>
      <th className="hidden md:table-cell">Host</th>
      <th className="hidden text-right sm:table-cell">CPU</th>
      <th className="hidden text-right sm:table-cell">Allocated</th>
      <th className="hidden lg:table-cell">Profiles</th>
    </tr>
  );
}

/** A type's total in an upload, where it holds it. */
const totalOf = (u: ProfileUpload, types: string[]) => {
  for (const p of u.profiles) for (const t of p.types) if (types.includes(t.type)) return t;
  return null;
};

function Uploads({ range, q, onPick }: { range: Range; q: string; onPick: (id: string) => void }) {
  const { uploads, matched } = useRes(uploadsRes(range, q)).data;
  if (uploads.length === 0) return <p className="empty-line py-12 text-center">{q ? "No upload matches in this time." : "No upload in this time."}</p>;
  return (
    <>
      <div className="table-wrap">
        <table className="data-table">
          <thead>
            <UploadsHead />
          </thead>
          <tbody>
            {uploads.map((u) => (
              <UploadRow key={u.id} u={u} onPick={onPick} />
            ))}
          </tbody>
        </table>
      </div>
      <p className="mt-3 text-[12.5px] text-[#8c8f98]">
        {matched > uploads.length ? `The newest ${uploads.length} of ${matched.toLocaleString()}. ` : ""}
        CPU in cores over the upload's time; allocated, the bytes it allocated then. One chosen shows its own flame graph.
      </p>
    </>
  );
}

function UploadRow({ u, onPick }: { u: ProfileUpload; onPick: (id: string) => void }) {
  const tag = (k: string) => u.tags.find((t) => t.startsWith(`${k}:`))?.slice(k.length + 1);
  const cpu = totalOf(u, ["cpu", "cpu-time"]);
  const alloc = totalOf(u, ["alloc_space", "alloc-space"]);
  const secs = Math.max(1e-9, (u.end_ms - u.start_ms) / 1000);
  return (
    <tr>
      <td className="truncate-cell">
        <button type="button" className="text-left font-mono text-[13px] text-[#e4e5e7] hover:text-white" onClick={() => onPick(u.id)}>
          {tag("service") ?? "—"}
          {tag("version") ? <span className="text-[#8c8f98]"> {tag("version")}</span> : null}
        </button>
        <p className="truncate text-[11px] text-[#8c8f98]">
          {new Date(u.start_ms).toLocaleString()}
          {tag("env") ? ` · ${tag("env")}` : ""} · {u.family}
          <span className="sm:hidden">{cpu ? ` · ${compact(cpu.total / 1e9 / secs)} cores` : ""}</span>
        </p>
      </td>
      <td className="hidden max-w-[12rem] truncate font-mono text-[12px] md:table-cell">{tag("host") ?? <span className="text-[#8c8f98]">—</span>}</td>
      <td className="hidden whitespace-nowrap text-right font-mono text-[12px] sm:table-cell">{cpu ? `${compact(cpu.total / 1e9 / secs)} cores` : <span className="text-[#8c8f98]">—</span>}</td>
      <td className="hidden whitespace-nowrap text-right font-mono text-[12px] sm:table-cell">{alloc ? formatBytes(alloc.total) : <span className="text-[#8c8f98]">—</span>}</td>
      <td className="hidden max-w-[16rem] truncate text-[12px] text-[#8c8f98] lg:table-cell">{u.profiles.map((p) => p.name).join(", ")}</td>
    </tr>
  );
}
