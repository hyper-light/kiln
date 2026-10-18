/**
 * A dashboard's widgets, each kind drawn from what it asks: series over the range (timeseries),
 * one number (query value), series ranked (toplist) or tabled, the newest log records or events,
 * monitors' states, or text.
 */
import * as React from "react";
import { Link } from "@/lib/router";
import type { Widget } from "@/api/observe";
import { SERIES } from "@/components/charts/types";
import { useRes } from "@/lib/data";
import {
  PALETTE,
  QueryChart,
  STATUS_COLOR,
  StateBadge,
  aggregate,
  dataOf,
  firstLine,
  formatValue,
  logsOnly,
  monitorsRes,
  queriesRes,
  recordsRes,
  seriesLabel,
  when,
  within,
  type Range,
} from "./parts";

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
  logs: "Log stream",
  events: "Event stream",
  monitors: "Monitors",
  note: "Note",
};

/** What a widget shows, its queries' template variables filled (`fill`). */
export function WidgetBody({ w, range, fill, fillSearch }: { w: Widget; range: Range; fill: (q: string) => string; fillSearch: (q: string) => string }) {
  const height = Math.max(ROW, w.layout.h * ROW - FRAME);
  const unit = w.options?.unit;
  const queries = w.queries.map(fill).filter((q) => q.trim());
  switch (w.kind) {
    case "timeseries":
      return queries.length ? <QueryChart queries={queries} range={range} height={height - 26} unit={unit} display={w.options?.display} /> : <Empty height={height} what="a query" />;
    case "query_value":
      return queries.length ? <QueryValue queries={queries} range={range} height={height} unit={unit} how={aggregator(w)} /> : <Empty height={height} what="a query" />;
    case "toplist":
      return queries.length ? <TopList queries={queries} range={range} height={height} unit={unit} how={aggregator(w)} /> : <Empty height={height} what="a query" />;
    case "table":
      return queries.length ? <SeriesTable queries={queries} range={range} height={height} unit={unit} /> : <Empty height={height} what="a query" />;
    case "logs":
      return <Stream query={logsOnly(fillSearch(w.search))} range={range} height={height} kind="logs" />;
    case "events":
      return <Stream query={within("source:kiln.event", fillSearch(w.search))} range={range} height={height} kind="events" />;
    case "monitors":
      return <Monitors filter={w.search} height={height} />;
    case "note":
      return (
        <div className="obs-note overflow-y-auto" style={{ maxHeight: height }}>
          <MarkdownBody text={w.text} />
        </div>
      );
  }
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
function QueryValue({ queries, range, height, unit, how }: { queries: string[]; range: Range; height: number; unit?: string; how: string }) {
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

/** Each series summed over the range, the largest first, as bars. */
function TopList({ queries, range, height, unit, how }: { queries: string[]; range: Range; height: number; unit?: string; how: string }) {
  const answers = useRes(queriesRes(queries, range)).data;
  const rows = answers
    .flatMap((a) => a.series.map((s) => ({ label: seriesLabel(s.tags), v: aggregate(dataOf(s), how) })))
    .filter((r): r is { label: string; v: number } => r.v !== null)
    .sort((a, b) => b.v - a.v)
    .slice(0, 25);
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

/** Each series' last, average, least and most over the range. */
function SeriesTable({ queries, range, height, unit }: { queries: string[]; range: Range; height: number; unit?: string }) {
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
export function Stream({ query, range, height, kind }: { query: string; range: Range; height: number; kind: "logs" | "events" }) {
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
