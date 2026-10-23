/**
 * A dashboard's widgets, each kind drawn from what it asks: series over the range (timeseries),
 * one number (query value), series ranked (toplist), tabled or as shares of a ring (pie), the
 * newest log records or events, monitors' states, a service check's status, or text. A group's
 * widgets are drawn by the dashboard, on a grid of the group's own.
 */
import * as React from "react";
import { Link } from "@/lib/router";
import type { CheckState, CheckStatusAsk, Widget } from "@/api/observe";
import { SERIES } from "@/components/charts/types";
import { useRes } from "@/lib/data";
import {
  CHECK_WORDS,
  CheckBadge,
  PALETTE,
  QueryChart,
  STATUS_COLOR,
  StateBadge,
  aggregate,
  checkStatusRes,
  dataOf,
  firstLine,
  formatValue,
  heatmapRes,
  logsOnly,
  monitorsRes,
  queriesRes,
  recordsRes,
  seriesLabel,
  when,
  within,
  type Span,
} from "./parts";

import { DistributionChart, HeatmapChart } from "./heatmap";

// Markdown's renderer is big: only a dashboard with a note loads it.
const MarkdownBody = React.lazy(() => import("@/lib/markdown").then((m) => ({ default: m.MarkdownBody })));

/** The rows of the grid a widget's `h` counts, in px. */
export const ROW = 76;
/** A widget's frame around its body: its title and padding. */
const FRAME = 58;

export const KIND_WORDS: Record<Widget["kind"], string> = {
  timeseries: "Timeseries",
  query_value: "Query value",
  toplist: "Top list",
  table: "Table",
  heatmap: "Heatmap",
  distribution: "Distribution",
  pie: "Pie",
  logs: "Log stream",
  events: "Event stream",
  monitors: "Monitors",
  check: "Check status",
  note: "Note",
  group: "Group",
};

/** What a widget shows, its queries' template variables filled (`fill`). */
export function WidgetBody({ w, range, fill, fillSearch }: { w: Widget; range: Span; fill: (q: string) => string; fillSearch: (q: string) => string }) {
  const height = Math.max(ROW, w.layout.h * ROW - FRAME);
  const unit = w.options?.unit;
  const queries = w.queries.map(fill).filter((q) => q.trim());
  switch (w.kind) {
    case "timeseries":
      return queries.length ? <QueryChart queries={queries} range={range} height={height - 26} unit={unit} display={w.options?.display} /> : <Empty height={height} what="a query" />;
    case "query_value":
      return queries.length ? <QueryValue queries={queries} range={range} height={height} unit={unit} how={aggregator(w)} /> : <Empty height={height} what="a query" />;
    case "toplist":
      return queries.length ? (
        <TopList queries={queries} range={range} height={height} unit={unit} how={aggregator(w)} limit={w.options?.limit} ascending={w.options?.order === "asc"} />
      ) : (
        <Empty height={height} what="a query" />
      );
    case "pie":
      return queries.length ? <Pie queries={queries} range={range} height={height} unit={unit} how={w.options?.aggregator ?? "sum"} /> : <Empty height={height} what="a query" />;
    case "table":
      return queries.length ? <SeriesTable queries={queries} range={range} height={height} unit={unit} /> : <Empty height={height} what="a query" />;
    case "heatmap":
      return queries[0] ? <Heat query={queries[0]} range={range} height={height - 18} unit={unit} /> : <Empty height={height} what="a query" />;
    case "distribution":
      return queries[0] ? <Spread query={queries[0]} range={range} height={height} unit={unit} /> : <Empty height={height} what="a query" />;
    case "logs":
      return <Stream query={logsOnly(fillSearch(w.search))} range={range} height={height} kind="logs" />;
    case "events":
      return <Stream query={within("source:kiln.event", fillSearch(w.search))} range={range} height={height} kind="events" />;
    case "monitors":
      return <Monitors filter={w.search} height={height} />;
    case "check":
      return w.options?.check ? <CheckStatusView w={w} range={range} height={height} fill={fill} /> : <Empty height={height} what="check" />;
    case "note":
      return (
        <div className="obs-note overflow-y-auto" style={{ maxHeight: height }}>
          <MarkdownBody text={w.text} />
        </div>
      );
    case "group":
      return null;
  }
}

/** The first query's heatmap over the range. */
function Heat({ query, range, height, unit }: { query: string; range: Span; height: number; unit?: string }) {
  const h = useRes(heatmapRes(query, range)).data;
  return <HeatmapChart h={h} height={height} unit={unit} day={range === "7d" || range === "30d"} />;
}

/** A plain metric query (`[agg:]metric[{…}][ by {…}]`) as its `p`th percentile's over all of its
 * series together (its groups' bars are counted together); none for another kind of query. */
export function asPercentile(query: string, p: string): string | null {
  const plain = /^\s*(?:[a-z]+[\d.]*:)?[\w.*?-]+(?:\{[^}]*\})?(?:\s+by\s+\{[^}]*\})?\s*$/i;
  if (!plain.test(query)) return null;
  const ungrouped = query.replace(/\s+by\s+\{[^}]*\}\s*$/i, "").trim();
  return /^[a-z]+[\d.]*:/i.test(ungrouped) ? ungrouped.replace(/^[a-z]+[\d.]*:/i, `${p}:`) : `${p}:${ungrouped}`;
}

/** The first query's values over the whole range, counted by their row, its median and 99th
 * percentile the server's. */
function Spread({ query, range, height, unit }: { query: string; range: Span; height: number; unit?: string }) {
  const h = useRes(heatmapRes(query, range, 40, 1)).data;
  const asked = ["p50", "p99"].map((p) => asPercentile(query, p));
  const plain = asked.filter((q): q is string => q !== null);
  const answers = useRes(queriesRes(plain, range, 1)).data;
  // The percentile of every series together: a single answer's own (the query's groups are bars here).
  const value = (i: number) => {
    const a = answers[i];
    const vs = (a?.series ?? []).flatMap((s) => s.values).filter((v): v is number => v !== null && Number.isFinite(v));
    return a && a.series.length === 1 ? (vs[0] ?? null) : null;
  };
  const percentiles =
    plain.length === 2
      ? [
          { label: "p50", v: value(0) },
          { label: "p99", v: value(1) },
        ]
      : [];
  return <DistributionChart h={h} height={height} unit={unit} percentiles={percentiles} />;
}

const aggregator = (w: Widget) => (w.options as { aggregator?: string } | null)?.aggregator ?? "avg";

function Empty({ height, what }: { height: number; what: string }) {
  return (
    <p className="empty-line flex items-center justify-center" style={{ minHeight: height }}>
      No {what} yet.
    </p>
  );
}

/** One number: the first query's series, each summed over the range, and those summed. */
function QueryValue({ queries, range, height, unit, how }: { queries: string[]; range: Span; height: number; unit?: string; how: string }) {
  const answers = useRes(queriesRes(queries, range)).data;
  const a = answers[0];
  const values = (a?.series ?? []).map((s) => aggregate(dataOf(s), how)).filter((v): v is number => v !== null);
  const v =
    a?.scalar ??
    (values.length
      ? how === "sum"
        ? values.reduce((x, y) => x + y, 0)
        : how === "max"
          ? Math.max(...values)
          : how === "min"
            ? Math.min(...values)
            : values.reduce((x, y) => x + y, 0) / values.length
      : null);
  return (
    <div className="flex items-center justify-center" style={{ minHeight: height }}>
      <span className="font-mono text-[clamp(28px,4vw,44px)] font-light tracking-[-0.03em] text-[#e9eaee] tabular-nums">{formatValue(v, unit)}</span>
    </div>
  );
}

/** Each series summed over the range, the largest first (the least, `ascending`), as bars: `limit`
 * of them, 25 unless the widget says. */
function TopList({ queries, range, height, unit, how, limit, ascending }: { queries: string[]; range: Span; height: number; unit?: string; how: string; limit?: number; ascending?: boolean }) {
  const answers = useRes(queriesRes(queries, range)).data;
  const rows = answers
    .flatMap((a) => a.series.map((s) => ({ label: seriesLabel(s.tags), v: aggregate(dataOf(s), how) })))
    .filter((r): r is { label: string; v: number } => r.v !== null)
    .sort((a, b) => (ascending ? a.v - b.v : b.v - a.v))
    .slice(0, Math.max(1, limit ?? 25));
  if (rows.length === 0) return <Empty height={height} what="data in this range" />;
  const top = Math.max(...rows.map((r) => Math.abs(r.v)), Number.MIN_VALUE);
  return (
    <ol className="space-y-1 overflow-y-auto" style={{ maxHeight: height }}>
      {rows.map((r, i) => (
        <li key={r.label} className="relative flex items-center justify-between gap-3 overflow-hidden rounded px-2 py-1 text-[12px]">
          <span className="absolute inset-y-0 left-0 rounded" style={{ width: `${(100 * Math.abs(r.v)) / top}%`, background: `${PALETTE[i % PALETTE.length] ?? SERIES.grey}26` }} aria-hidden="true" />
          <span className="relative min-w-0 truncate font-mono text-[11px] text-[#c8cad0]">{r.label}</span>
          <span className="relative font-mono text-[11px] text-[#e4e5e7] tabular-nums">{formatValue(r.v, unit)}</span>
        </li>
      ))}
    </ol>
  );
}

/** A pie's hues: kiln's violet, blue and rose stepped for its dark panels (OKLCH L 0.52–0.64, C ≥
 * 0.10). In a ring any two slices may touch, so every pair is held apart, and three are the most
 * that are: worst CVD ΔE 10.3 (8 the target), worst normal-vision ΔE 16.6 (15 the floor), 15.1
 * beside the grey "Other", each ≥ 3:1 on the panel (the dataviz validator, all pairs). */
const PIE_HUES = ["#9f71dc", "#416aa2", "#994e5d"];
/** Slices a pie draws at the most: the largest three, the rest one ("Other", grey). */
const SLICES = PIE_HUES.length + 1;

/** Each series over the range (summed unless the widget says) as its share of a ring, each slice
 * named in the legend with its value and share; a slice's hue follows its series, not its rank. */
function Pie({ queries, range, height, unit, how }: { queries: string[]; range: Span; height: number; unit?: string; how: string }) {
  const answers = useRes(queriesRes(queries, range)).data;
  const rows = answers
    .flatMap((a) => a.series.map((s) => ({ label: seriesLabel(s.tags), v: aggregate(dataOf(s), how) })))
    .filter((r): r is { label: string; v: number } => r.v !== null && r.v > 0)
    .sort((a, b) => b.v - a.v);
  if (rows.length === 0) return <Empty height={height} what="data in this range" />;
  const shown = rows.length > SLICES ? [...rows.slice(0, SLICES - 1), { label: "Other", v: rows.slice(SLICES - 1).reduce((x, r) => x + r.v, 0) }] : rows;
  const total = shown.reduce((x, r) => x + r.v, 0);
  const hue = hues(shown.filter((r) => !(r.label === "Other" && rows.length > SLICES)).map((r) => r.label));
  const size = Math.max(64, Math.min(height, 168));
  const outer = size / 2;
  let at = 0;
  const slices = shown.map((r) => {
    const from = at;
    at += r.v / total;
    return { ...r, d: ring(outer, outer * 0.62, from, at), color: r.label === "Other" && rows.length > SLICES ? SERIES.grey : (hue.get(r.label) ?? SERIES.grey) };
  });
  return (
    <div className="flex flex-wrap items-center gap-4 overflow-y-auto" style={{ maxHeight: height }}>
      <svg width={size} height={size} viewBox={`${-outer} ${-outer} ${size} ${size}`} role="img" aria-label={queries.join("; ")} className="shrink-0">
        {slices.map((sl) => (
          <path key={sl.label} d={sl.d} fill={sl.color} stroke="#0a0c0f" strokeWidth={2}>
            <title>{`${sl.label}: ${formatValue(sl.v, unit)}`}</title>
          </path>
        ))}
      </svg>
      <ul className="min-w-0 flex-1 space-y-1">
        {slices.map((sl) => (
          <li key={sl.label} className="flex items-center gap-2">
            <span className="size-2 shrink-0 rounded-full" style={{ background: sl.color }} aria-hidden="true" />
            <span className="min-w-0 flex-1 truncate font-mono text-[11px] text-[#c8cad0]">{sl.label}</span>
            <span className="font-mono text-[11px] text-[#e4e5e7] tabular-nums">{formatValue(sl.v, unit)}</span>
            <span className="w-9 text-right font-mono text-[11px] text-[#8c8f98] tabular-nums">{Math.round((100 * sl.v) / total)}%</span>
          </li>
        ))}
      </ul>
    </div>
  );
}

/** The hued slices' hues, in their labels' order: a series keeps its hue as its rank moves (a
 * change of which series are shown may move them). */
function hues(labels: string[]): Map<string, string> {
  return new Map([...labels].sort().map((label, i) => [label, PIE_HUES[i] ?? SERIES.grey]));
}

/** A ring's slice from share `a` to share `b` of the turn, clockwise from the top; a whole ring as
 * two halves (an arc cannot end where it starts). */
function ring(outer: number, inner: number, a: number, b: number): string {
  if (b - a > 0.9999) return `${ring(outer, inner, a, a + 0.5)} ${ring(outer, inner, a + 0.5, a + 0.9999)}`;
  const at = (t: number, rad: number) => `${(rad * Math.sin(2 * Math.PI * t)).toFixed(2)} ${(-rad * Math.cos(2 * Math.PI * t)).toFixed(2)}`;
  const large = b - a > 0.5 ? 1 : 0;
  return `M${at(a, outer)} A${outer} ${outer} 0 ${large} 1 ${at(b, outer)} L${at(b, inner)} A${inner} ${inner} 0 ${large} 0 ${at(a, inner)} Z`;
}

/** Each series' last, average, least and most over the range. */
function SeriesTable({ queries, range, height, unit }: { queries: string[]; range: Span; height: number; unit?: string }) {
  const answers = useRes(queriesRes(queries, range)).data;
  const rows = answers.flatMap((a) =>
    a.series.map((s) => ({ label: seriesLabel(s.tags), last: aggregate(dataOf(s), "last"), avg: aggregate(dataOf(s), "avg"), min: aggregate(dataOf(s), "min"), max: aggregate(dataOf(s), "max") })),
  );
  if (rows.length === 0) return <Empty height={height} what="data in this range" />;
  return (
    <div className="overflow-auto" style={{ maxHeight: height }}>
      <table className="data-table">
        <thead>
          <tr>
            <th>Series</th>
            <th className="num">Last</th>
            <th className="num">Avg</th>
            <th className="num">Min</th>
            <th className="num">Max</th>
          </tr>
        </thead>
        <tbody>
          {rows.map((r) => (
            <tr key={r.label}>
              <td className="cell-mono">{r.label}</td>
              <td className="num cell-mono">{formatValue(r.last, unit)}</td>
              <td className="num cell-mono">{formatValue(r.avg, unit)}</td>
              <td className="num cell-mono">{formatValue(r.min, unit)}</td>
              <td className="num cell-mono">{formatValue(r.max, unit)}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

/** The newest records a search finds. */
export function Stream({ query, range, height, kind }: { query: string; range: Span; height: number; kind: "logs" | "events" }) {
  const found = useRes(recordsRes(query, range, 50)).data;
  if (found.records.length === 0) return <Empty height={height} what={kind === "events" ? "events in this range" : "records in this range"} />;
  return (
    <ol className="divide-y divide-[#1d1f23] overflow-y-auto" style={{ maxHeight: height }}>
      {found.records.map((r, i) => (
        <li key={`${r.ts_ns}-${i}`} className="flex items-baseline gap-2.5 py-1.5">
          <span className="inline-block size-1.5 shrink-0 translate-y-[-1px] rounded-full" style={{ background: STATUS_COLOR[r.status] }} aria-label={r.status} />
          <time className="shrink-0 font-mono text-[10px] text-[#8c8f98] tabular-nums">{when(r.ts_ns)}</time>
          <span className="min-w-0 truncate text-[12px] text-[#d6d8dd]">{firstLine(r.message)}</span>
        </li>
      ))}
    </ol>
  );
}

/** Monitors whose name or tags hold `filter` (all of them for none), in trouble first. */
const CHECK_ORDER: CheckState[] = ["CRITICAL", "WARNING", "UNKNOWN", "OK"];

/** A check widget: its tags' template variables filled (one of a variable stands for its value, or
 * for any: `*`, left out). */
function CheckStatusView({ w, range, height, fill }: { w: Widget; range: Span; height: number; fill: (q: string) => string }) {
  const o = w.options ?? {};
  const one = o.grouping === "check";
  const tags = (o.tags ?? []).map((t) => fill(t).trim()).filter((t) => t && t !== "*");
  return <CheckRuns ask={{ check: o.check ?? "", tags, group_by: o.group_by ?? [], group: one ? fill(o.group ?? "") : undefined }} range={range} height={height} one={one} />;
}

/** A service check's status over the range (Datadog's check status widget's): of every group, the
 * worst, how many are in each status, and those not OK by name; or (`one`) one group's, and when it
 * last ran. */
export function CheckRuns({ ask, range, height, one }: { ask: CheckStatusAsk; range: Span; height: number; one?: boolean }) {
  const s = useRes(checkStatusRes(ask, range)).data;
  if (!s.status) return <Empty height={height} what={`runs of ${s.check}`} />;
  const named = (t: Record<string, string>) =>
    Object.entries(t)
      .map(([k, v]) => `${k}:${v}`)
      .join(", ") || s.check;
  const first = s.groups[0];
  const counts = CHECK_ORDER.filter((k) => s.counts[k] > 0)
    .map((k) => `${s.counts[k]} ${CHECK_WORDS[k]}`)
    .join(" · ");
  const trouble = s.groups.filter((g) => g.status !== "OK").sort((a, b) => CHECK_ORDER.indexOf(a.status) - CHECK_ORDER.indexOf(b.status));
  return (
    <div className="flex flex-col gap-1.5 overflow-y-auto" style={{ maxHeight: height }}>
      <CheckBadge status={s.status} className="text-[18px]" />
      <p className="text-[12px] text-[#8c8f98]">{one && first ? `${named(first.tags)} · ${when(first.at * 1e6)}` : counts}</p>
      {!one && trouble.length > 0 && (
        <ul className="divide-y divide-[#1d1f23]" aria-label="Groups not OK">
          {trouble.map((g) => (
            <li key={named(g.tags)} className="flex items-center justify-between gap-3 py-1">
              <span className="min-w-0 truncate font-mono text-[12px] text-[#d6d8dd]">{named(g.tags)}</span>
              <CheckBadge status={g.status} />
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

function Monitors({ filter, height }: { filter: string; height: number }) {
  const all = useRes(monitorsRes).data;
  const f = filter.trim().toLowerCase();
  const rank: Record<string, number> = { ALERT: 0, WARN: 1, "NO DATA": 2, OK: 3 };
  const shown = all
    .filter((m) => !f || m.name.toLowerCase().includes(f) || m.tags.some((t) => t.toLowerCase().includes(f)))
    .sort((a, b) => (rank[a.state] ?? 4) - (rank[b.state] ?? 4) || a.name.localeCompare(b.name));
  if (shown.length === 0) return <Empty height={height} what="monitors" />;
  const now = Date.now();
  return (
    <ul className="divide-y divide-[#1d1f23] overflow-y-auto" style={{ maxHeight: height }}>
      {shown.map((m) => (
        <li key={m.id} className="flex items-center justify-between gap-3 py-1.5">
          <Link to={`/observe/monitors/${encodeURIComponent(m.id)}`} className="min-w-0 truncate text-[12.5px] text-[#d6d8dd] hover:text-white">
            {m.name}
          </Link>
          <StateBadge state={m.state} muted={m.muted_until !== null && m.muted_until > now} />
        </li>
      ))}
    </ul>
  );
}
