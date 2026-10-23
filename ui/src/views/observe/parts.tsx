/**
 * What every Observe page shares: its sections' tabs, the time range (in the URL, so a page is a
 * link), what a query answers over a range, and how it is drawn.
 */
import * as React from "react";
import { NavLink, useSearchParams } from "@/lib/router";
import {
  observeApi,
  type Bounds,
  type Cloud,
  type Dashboard,
  type DashboardSummary,
  type Integration,
  type LogHistogram,
  type LogSearch,
  type Monitor,
  type ObserveStatus,
  type QueryAnswer,
  type Scrape,
  type Statsd,
  type Synthetic,
  type HeatmapAnswer,
  type Slo,
  type LogSource,
  type Pipeline,
  type Archive,
  type Rehydration,
  type ScanningGroup,
  type StandardPattern,
  type LogMetric,
  type Downtime,
  type LogMetricCounting,
  type CheckState,
  type CheckStatus,
  type CheckStatusAsk,
} from "@/api/observe";
import { Chart } from "@/components/charts/chart";
import { SERIES, type Band } from "@/components/charts/types";
import { Segmented, tabsTriggerClass } from "@/components/ui/tabs";
import { useRes, type Res } from "@/lib/data";
import { formatBytes, formatDuration } from "@/lib/format";

const SECTIONS = [
  { to: "/observe", label: "Overview", end: true },
  { to: "/observe/dashboards", label: "Dashboards" },
  { to: "/observe/notebooks", label: "Notebooks" },
  { to: "/observe/metrics", label: "Metrics" },
  { to: "/observe/hosts", label: "Hosts" },
  { to: "/observe/containers", label: "Containers" },
  { to: "/observe/logs", label: "Logs" },
  { to: "/observe/apm", label: "APM" },
  { to: "/observe/catalog", label: "Catalog" },
  { to: "/observe/errors", label: "Errors" },
  { to: "/observe/events", label: "Events" },
  { to: "/observe/monitors", label: "Monitors" },
  { to: "/observe/incidents", label: "Incidents" },
  { to: "/observe/slos", label: "SLOs" },
  { to: "/observe/delivery", label: "Delivery" },
  { to: "/observe/synthetics", label: "Synthetics" },
  { to: "/observe/integrations", label: "Integrations" },
  { to: "/observe/live", label: "Live" },
];

/** The sections of Observe, the current one marked. */
export function ObserveNav() {
  return (
    <nav aria-label="Observe" className="mb-8 flex max-w-full gap-1 overflow-x-auto rounded-[22px] border border-white/10 bg-white/[0.02] p-1 sm:inline-flex">
      {SECTIONS.map((s) => (
        <NavLink key={s.to} to={s.to} end={s.end} className={`${tabsTriggerClass} aria-[current=page]:border-white/10 aria-[current=page]:bg-white/[0.07] aria-[current=page]:text-[#f2f2f3]`}>
          {s.label}
        </NavLink>
      ))}
    </nav>
  );
}

export const RANGES = { "15m": 15 * 60_000, "1h": 3_600_000, "4h": 4 * 3_600_000, "1d": 86_400_000, "7d": 7 * 86_400_000, "30d": 30 * 86_400_000 } as const;
export type Range = keyof typeof RANGES;
const isRange = (r: string | null): r is Range => r !== null && r in RANGES;

/** A search param of the page's (`q`, `range`), and a way to change it (in place: no new entry in history). */
export function useParam(name: string, fallback = ""): [string, (v: string) => void] {
  const [params, setParams] = useSearchParams();
  const value = params.get(name) ?? fallback;
  const set = (v: string) =>
    React.startTransition(() =>
      setParams(
        (p) => {
          if (v === fallback) p.delete(name);
          else p.set(name, v);
          return p;
        },
        { replace: true },
      ),
    );
  return [value, set];
}

/** A way to change several of the page's search params at once (in place): two `useParam` setters
 * called together would each start from the params as they were, the second undoing the first. A
 * param set to `null` is taken out. */
export function useParams(): (changes: Record<string, string | null>) => void {
  const [, setParams] = useSearchParams();
  return (changes) =>
    React.startTransition(() =>
      setParams(
        (p) => {
          for (const [k, v] of Object.entries(changes)) {
            if (v === null) p.delete(k);
            else p.set(k, v);
          }
          return p;
        },
        { replace: true },
      ),
    );
}

/** The page's time range (`?range=`): the last hour unless it says. */
export function useRange(fallback: Range = "1h"): [Range, (r: Range) => void] {
  const [r, set] = useParam("range", fallback);
  return [isRange(r) ? r : fallback, set];
}

export function RangePicker({ value, onChange }: { value: Range; onChange: (r: Range) => void }) {
  return <Segmented label="Time range" value={value} onChange={onChange} options={(Object.keys(RANGES) as Range[]).map((r) => ({ value: r, label: r }))} />;
}

/** How often a range's answers are read again while shown: a 120th of it, between 15 s and 5 min. */
export const pollFor = (r: Range) => Math.max(15_000, Math.min(300_000, RANGES[r] / 120));

/** A time: the last of a range, or from one time to another (ms: a notebook's, a postmortem's). */
export type Span = Range | { from: number; to: number };
export const spanKey = (s: Span) => (typeof s === "string" ? s : `${s.from}-${s.to}`);
export const spanMs = (s: Span) => (typeof s === "string" ? RANGES[s] : s.to - s.from);
/** Its ends now: the last of a range ends now; a fixed time is as it is. */
export function spanOf(s: Span): { from: number; to: number } {
  if (typeof s !== "string") return s;
  const now = Date.now();
  return { from: now - RANGES[s], to: now };
}
/** A range's answers are read again as it moves on; a fixed time's are not. */
const pollOf = (s: Span) => (typeof s === "string" ? pollFor(s) : undefined);

/** What `query` answers over the last `range`, read again as the range moves on. */
/** How far past now a chart of these queries reaches: half its range when one forecasts (a day's
 * chart its next twelve hours), unless asked; none else. */
export function aheadOf(queries: string[], range: Span, ask?: number): number {
  if (!queries.some((q) => /\bforecast\s*\(/i.test(q))) return 0;
  return ask ?? (typeof range === "string" ? RANGES[range] / 2 : 0);
}

export const queryRes = (query: string, range: Span, points = 240): Res<QueryAnswer> => ({
  key: `observe/query/${spanKey(range)}/${points}/${query}`,
  fetch: (signal) => {
    const { from, to } = spanOf(range);
    return observeApi.query({ query, from, to: to + aheadOf([query], range), points }, signal);
  },
  opts: { pollMs: pollOf(range) },
});

/** A query's heatmap over the last `range`: `rows` rows at the most, `points` buckets of time. */
export const heatmapRes = (query: string, range: Span, rows = 40, points = 120): Res<HeatmapAnswer> => ({
  key: `observe/heatmap/${spanKey(range)}/${rows}/${points}/${query}`,
  fetch: (signal) => observeApi.heatmap({ query, ...spanOf(range), points, heatmap: rows }, signal),
  opts: { pollMs: pollOf(range) },
});

/** A service check's status over the last `range` (a check widget's): each group's latest. */
export const checkStatusRes = (ask: CheckStatusAsk, range: Span): Res<CheckStatus> => ({
  key: `observe/checks/${spanKey(range)}/${JSON.stringify(ask)}`,
  fetch: (signal) => observeApi.checkStatus({ ...ask, ...spanOf(range) }, signal),
  opts: { pollMs: pollOf(range) },
});

/** What several queries answer over one range, read together (a widget's, the explorer's). */
export const queriesRes = (queries: string[], range: Span, points = 240, ahead?: number): Res<QueryAnswer[]> => ({
  key: `observe/queries/${spanKey(range)}/${points}/${ahead ?? ""}/${queries.join("\n")}`,
  fetch: (signal) => {
    const { from, to } = spanOf(range);
    const until = to + aheadOf(queries, range, ahead);
    return Promise.all(queries.map((query) => observeApi.query({ query, from, to: until, points }, signal)));
  },
  opts: { pollMs: pollOf(range) },
});

/** A series' data: its values up to where a forecast starts (all of them, without one). */
export function dataOf(s: QueryAnswer["series"][number]): (number | null)[] {
  const f = s.bounds?.forecast_from;
  return f === null || f === undefined ? s.values : s.values.slice(0, f);
}

/** A log search's records over the range, by status, in about 60 buckets of a round size. */
export const histogramRes = (query: string, range: Span): Res<LogHistogram> => ({
  key: `observe/histogram/${spanKey(range)}/${query}`,
  fetch: (signal) => observeApi.histogram({ query, ...spanOf(range), step: niceStep(spanMs(range) / 60), by: ["status"] }, signal),
  opts: { pollMs: pollOf(range) },
});

/** A span search's spans over the range, by status, as `histogramRes` has a log search's. */
export const spanHistogramRes = (query: string, range: Range): Res<LogHistogram> => ({
  key: `observe/span-histogram/${range}/${query}`,
  fetch: (signal) => {
    const to = Date.now();
    return observeApi.spanHistogram({ query, from: to - RANGES[range], to, step: niceStep(RANGES[range] / 60), by: ["status"] }, signal);
  },
  opts: { pollMs: pollFor(range) },
});

const STEPS = [1, 2, 5, 10, 15, 30, 60, 120, 300, 600, 900, 1800, 3600, 7200, 10800, 21600, 43200, 86400].map((s) => s * 1000);

/** The smallest round step (1 s, 5 s, 1 min, 1 h, ...) of at least `ms`. */
export function niceStep(ms: number): number {
  return STEPS.find((s) => s >= ms) ?? 86_400_000 * Math.ceil(ms / 86_400_000);
}

/** A series' name: its tags (`host:a, mount:/`), or the query's when it has none. */
export function seriesLabel(tags: Record<string, string>, fallback = "*"): string {
  const parts = Object.entries(tags).map(([k, v]) => `${k}:${v}`);
  return parts.length ? parts.join(", ") : fallback;
}

export const PALETTE = [SERIES.violet, SERIES.blue, SERIES.teal, SERIES.sand, SERIES.rose, SERIES.ok, SERIES.pending, SERIES.warn, SERIES.fail, SERIES.grey];

/** A value as a unit reads: bytes, seconds, percent, a rate, or a plain number. */
export function formatValue(v: number | null | undefined, unit?: string): string {
  if (v === null || v === undefined || !Number.isFinite(v)) return "—";
  switch (unit) {
    case "B":
      return formatBytes(v);
    case "B/s":
      return `${formatBytes(v)}/s`;
    case "MiB":
      return formatBytes(v * 1024 * 1024);
    case "s":
      return formatSeconds(v);
    case "%":
      return `${compact(v)}%`;
    case "fraction":
      return `${compact(v * 100)}%`;
    default:
      return unit ? `${compact(v)} ${unit}` : compact(v);
  }
}

/** A metric's seconds: a latency's ns, µs or ms below a second (0.052 → "52 ms"), seconds below a
 * minute, then as a job's duration reads ("4m 12s"). */
export function formatSeconds(v: number): string {
  const a = Math.abs(v);
  if (a === 0) return "0 s";
  if (a < 1e-6) return `${compact(v * 1e9)} ns`;
  if (a < 1e-3) return `${compact(v * 1e6)} µs`;
  if (a < 1) return `${compact(v * 1e3)} ms`;
  if (a < 60) return `${compact(v)} s`;
  return v < 0 ? `-${formatDuration(-v)}` : formatDuration(v);
}

/** 1234567 → 1.23M, 0.000123 → 1.23e-4: a number in a few characters. */
export function compact(v: number): string {
  const a = Math.abs(v);
  if (a === 0) return "0";
  if (a >= 1e15 || a < 1e-3) return v.toExponential(2);
  if (a >= 1e3) return new Intl.NumberFormat(undefined, { notation: "compact", maximumFractionDigits: 2 }).format(v);
  return String(Number(v.toPrecision(3)));
}

/** A series' values summed over the range: what a query value or a toplist shows. */
export function aggregate(values: (number | null)[], how: string = "avg"): number | null {
  const xs = values.filter((v): v is number => v !== null && Number.isFinite(v));
  if (xs.length === 0) return null;
  switch (how) {
    case "sum":
      return xs.reduce((a, b) => a + b, 0);
    case "min":
      return Math.min(...xs);
    case "max":
      return Math.max(...xs);
    case "last":
      return xs[xs.length - 1] ?? null;
    default:
      return xs.reduce((a, b) => a + b, 0) / xs.length;
  }
}

/** The time axis a range (or a span of ms) reads best in. */
const timeFormat = (r: Span | number) => ((typeof r === "number" ? r : spanMs(r)) > 2 * 86_400_000 ? "day" : "time");

/** Answers drawn as uPlot takes them: the first answer's buckets, and every series on them. */
/** Whether an `anomalies()` score is past its band, on the side it watches. */
function past(score: number | null, k: number, direction: Bounds["direction"]) {
  if (score == null) return false;
  return direction === "above" ? score > k : direction === "below" ? score < -k : Math.abs(score) > k;
}

/** A drawn series: its label, its source series' place (its color), whether it is a forecast, and,
 * an `outliers()` series', whether it is an outlier (one that is not is drawn grey). */
export interface Drawn {
  label: string;
  source: number;
  forecast: boolean;
  outlier?: boolean;
}

/** A drawn series' color: its palette color, or grey for an `outliers()` series that is not one. */
export function drawnColor(d: Drawn): string {
  return d.outlier === false ? SERIES.grey : (PALETTE[d.source % PALETTE.length] ?? SERIES.grey);
}

/** Bucket `i`'s start (ms): a calendar rollup's own, or the step's. */
const bucketTime = (g: QueryAnswer["grid"], i: number) => g.times?.[i] ?? g.from + i * g.step;

/** Answers as a chart's columns: each series as a line; a `forecast()` series as two, its data
 * and, from its last point on, its forecast (drawn dashed, its band about it). */
/** A series' samples' usual spacing in buckets: the median between neighbours (one when dense, or a
 * lone value). */
export function spacingOf(values: (number | null)[]): number {
  const apart: number[] = [];
  let last = -1;
  values.forEach((v, i) => {
    if (v === null) return;
    if (last >= 0) apart.push(i - last);
    last = i;
  });
  apart.sort((a, b) => a - b);
  return apart[Math.floor(apart.length / 2)] ?? 1;
}

/** A chart's series and labels; each query's named by `names` where given (else by its place). */
export function chartData(answers: QueryAnswer[], names?: readonly string[]): { data: (number | null)[][]; labels: string[]; bands: Band[]; drawn: Drawn[] } {
  const first = answers[0];
  if (!first) return { data: [[]], labels: [], bands: [], drawn: [] };
  const xs = Array.from({ length: first.grid.n }, (_, i) => bucketTime(first.grid, i) / 1000);
  const ys: (number | null)[][] = [];
  const labels: string[] = [];
  const bands: Band[] = [];
  const drawn: Drawn[] = [];
  let source = 0;
  answers.forEach((a, qi) => {
    const same = a.grid.from === first.grid.from && a.grid.step === first.grid.step && a.grid.calendar === first.grid.calendar;
    for (const s of a.series) {
      const name = names?.[qi];
      const label = name !== undefined ? (a.series.length > 1 ? `${name}: ${seriesLabel(s.tags)}` : name) : answers.length > 1 ? `${qi + 1}: ${seriesLabel(s.tags)}` : seriesLabel(s.tags);
      const b = s.bounds;
      let values: (number | null)[];
      if (same) values = s.values.slice(0, xs.length);
      else {
        // Another grid (its read came a bucket later): each bucket at its time on the first's.
        const at = new Map<number, number | null>();
        s.values.forEach((v, i) => at.set(bucketTime(a.grid, i) / 1000, v));
        values = xs.map((x) => at.get(x) ?? null);
      }
      const from = b?.forecast_from;
      if (b && same && from !== null && from !== undefined) {
        ys.push(values.map((v, i) => (i < from ? v : null)));
        labels.push(label);
        drawn.push({ label, source, forecast: false });
        // From the last data point, so the two lines meet.
        bands.push({ series: ys.length, lower: b.lower.slice(0, xs.length), upper: b.upper.slice(0, xs.length) });
        ys.push(values.map((v, i) => (i >= from - 1 ? v : null)));
        labels.push(`${label} · forecast`);
        drawn.push({ label: `${label} · forecast`, source, forecast: true });
      } else {
        if (b && same)
          bands.push({ series: ys.length, lower: b.lower.slice(0, xs.length), upper: b.upper.slice(0, xs.length), anomalous: b.score.slice(0, xs.length).map((v) => past(v, b.k, b.direction)) });
        ys.push(values);
        labels.push(label);
        drawn.push({ label, source, forecast: false, outlier: s.outlier });
      }
      source += 1;
    }
  });
  return { data: [xs, ...ys], labels, bands, drawn };
}

/** A level drawn across a chart: a monitor's threshold. */
export interface Mark {
  label: string;
  value: number;
  color: string;
}

/** One query's (or a few's) series over the range, drawn as lines, areas or bars; `marks` across it. */
export function QueryChart({
  queries,
  range,
  height = 180,
  unit,
  display = "line",
  legend,
  marks = [],
  ahead,
  names,
}: {
  queries: string[];
  range: Span;
  height?: number;
  unit?: string;
  display?: "line" | "bars" | "area";
  legend?: boolean;
  marks?: Mark[];
  /** How far past now to draw a forecast (ms): half the range unless said. */
  ahead?: number;
  /** Each query's name in the legend (a constant: it is compared by identity). */
  names?: readonly string[];
}) {
  const answers = useRes(queriesRes(queries, range, 240, ahead)).data;
  const { data, labels, bands, drawn } = React.useMemo(() => chartData(answers, names), [answers, names]);
  // Each series' spacing (a metric sent less often than the grid's step drawn across its empty
  // buckets), and whether it has one value alone (a point, there being no line).
  const sparse = React.useMemo(() => data.slice(1).map((ys) => ({ spacing: spacingOf(ys), lone: ys.filter((v) => v !== null).length === 1 })), [data]);
  if (labels.length === 0)
    return (
      <p className="empty-line flex items-center justify-center" style={{ minHeight: height }}>
        No data in this range.
      </p>
    );
  const xs = data[0] ?? [];
  return (
    <Chart
      ariaLabel={queries.join("; ")}
      data={[...data, ...marks.map((m) => xs.map(() => m.value))]}
      height={height}
      timeFormat={timeFormat(range)}
      yFormat={(v) => formatValue(v, unit)}
      legend={legend ?? labels.length + marks.length <= 6}
      bands={bands}
      series={[
        ...drawn.map((d, i) => ({
          label: d.label,
          color: drawnColor(d),
          bars: display === "bars" && !d.forecast,
          fill: display === "area" && !d.forecast,
          spacing: sparse[i]?.spacing,
          points: sparse[i]?.lone,
          ...(d.forecast ? { dash: [6, 4] } : {}),
        })),
        ...marks.map((m) => ({ label: m.label, color: m.color, dash: [4, 4], width: 1 })),
      ]}
    />
  );
}

/** Each status's color, worst last (drawn last: on top). */
export const STATUS_ORDER = ["unspecified", "trace", "debug", "info", "warn", "error", "fatal"];
export const STATUS_COLOR: Record<string, string> = {
  trace: SERIES.grey,
  debug: SERIES.grey,
  info: SERIES.pending,
  warn: SERIES.warn,
  error: SERIES.fail,
  fatal: SERIES.rose,
  unspecified: SERIES.grey,
};

/**
 * A log histogram as stacked bars: each status's bar is drawn as the sum of it and every status
 * worse than it, the mildest first, so each one shows as its own segment; the legend says each
 * one's own count.
 */
export function Histogram({ h, range, height = 120 }: { h: LogHistogram; range: Span | number; height?: number }) {
  const { data, series } = React.useMemo(() => {
    const xs = Array.from({ length: h.grid.n }, (_, i) => (h.grid.from + i * h.grid.step) / 1000);
    const groups = [...h.series].sort((a, b) => STATUS_ORDER.indexOf(a.group.status ?? "") - STATUS_ORDER.indexOf(b.group.status ?? ""));
    const own = groups.map((g) => xs.map((_, i) => g.values[i] ?? 0));
    const stacked = own.map((_, gi) => xs.map((_, i) => own.slice(gi).reduce((a, s) => a + (s[i] ?? 0), 0)));
    return {
      data: [xs, ...stacked.map((s) => s.map((v) => (v === 0 ? null : v)))],
      series: groups.map((g, gi) => ({
        label: g.group.status ?? "—",
        color: STATUS_COLOR[g.group.status ?? ""] ?? SERIES.grey,
        bars: true,
        value: (_d: unknown, _v: number | null, _si: number, i: number) => String(own[gi]?.[i] ?? 0),
      })),
    };
  }, [h]);
  if (series.length === 0)
    return (
      <p className="empty-line flex items-center justify-center" style={{ minHeight: height }}>
        Nothing in this range.
      </p>
    );
  return <Chart ariaLabel="Records over time, by status" data={data} height={height} timeFormat={timeFormat(range)} yFormat={(v) => compact(v)} series={series} />;
}

/* What the server keeps, each under one key: the pages that show it share it. */

export const statusRes: Res<ObserveStatus> = { key: "observe/status", fetch: (s) => observeApi.status(s), opts: { pollMs: 15_000 } };
export const monitorsRes: Res<Monitor[]> = { key: "observe/monitors", fetch: (s) => observeApi.monitors(s), opts: { pollMs: 30_000 } };
export const downtimesRes: Res<Downtime[]> = { key: "observe/downtimes", fetch: (s) => observeApi.downtimes(s), opts: { pollMs: 30_000 } };
// One item's key is not under its list's: a list refreshed after a delete must not refresh the item deleted.
export const monitorRes = (id: string): Res<Monitor> => ({ key: `observe/monitor/${id}`, fetch: (s) => observeApi.monitor(id, s), opts: { pollMs: 30_000 } });
export const slosRes: Res<Slo[]> = { key: "observe/slos", fetch: (s) => observeApi.slos(s), opts: { pollMs: 60_000 } };
export const sloRes = (id: string): Res<Slo> => ({ key: `observe/slo/${id}`, fetch: (s) => observeApi.slo(id, s), opts: { pollMs: 60_000 } });
export const integrationsRes: Res<Integration[]> = { key: "observe/integrations", fetch: (s) => observeApi.integrations(s) };
export const cloudsRes: Res<Cloud[]> = { key: "observe/clouds", fetch: (s) => observeApi.clouds(s), opts: { pollMs: 10_000 } };
export const scrapesRes: Res<Scrape[]> = { key: "observe/scrapes", fetch: (s) => observeApi.scrapes(s), opts: { pollMs: 10_000 } };
export const statsdRes: Res<Statsd[]> = { key: "observe/statsd", fetch: (s) => observeApi.statsd(s), opts: { pollMs: 10_000 } };
export const syntheticsRes: Res<Synthetic[]> = { key: "observe/synthetics", fetch: (s) => observeApi.synthetics(s), opts: { pollMs: 10_000 } };
export const logSourcesRes: Res<LogSource[]> = { key: "observe/log_sources", fetch: (s) => observeApi.logSources(s), opts: { pollMs: 10_000 } };
export const pipelinesRes: Res<Pipeline[]> = { key: "observe/pipelines", fetch: (s) => observeApi.pipelines(s) };
export const archivesRes: Res<Archive[]> = { key: "observe/archives", fetch: (s) => observeApi.archives(s), opts: { pollMs: 10_000 } };
export const rehydrationsRes: Res<Rehydration[]> = { key: "observe/rehydrations", fetch: (s) => observeApi.rehydrations(s), opts: { pollMs: 5_000 } };
export const scanningGroupsRes: Res<ScanningGroup[]> = { key: "observe/scanner/groups", fetch: (s) => observeApi.scanningGroups(s) };
export const scanningGroupRes = (id: string): Res<ScanningGroup> => ({ key: `observe/scanner/group/${id}`, fetch: (s) => observeApi.scanningGroup(id, s) });
export const standardPatternsRes: Res<StandardPattern[]> = { key: "observe/scanner/standard_patterns", fetch: (s) => observeApi.standardPatterns(s) };
export const logMetricsRes: Res<{ log_metrics: LogMetric[]; counting: LogMetricCounting }> = { key: "observe/log_metrics", fetch: (s) => observeApi.logMetrics(s), opts: { pollMs: 10_000 } };
export const pipelineRes = (id: string): Res<Pipeline> => ({ key: `observe/pipeline/${id}`, fetch: (s) => observeApi.pipeline(id, s) });
export const dashboardsRes: Res<DashboardSummary[]> = { key: "observe/dashboards", fetch: (s) => observeApi.dashboards(s) };
export const dashboardRes = (id: string): Res<Dashboard> => ({ key: `observe/dashboard/${id}`, fetch: (s) => observeApi.dashboard(id, s) });
export const tagValuesRes = (tag: string): Res<string[]> => ({ key: `observe/tags/${tag}`, fetch: (s) => observeApi.tagValues(tag, s).then((r) => r.values), opts: { pollMs: 300_000 } });

/** The newest records a search finds over the range (a dashboard's log and event widgets). */
export const recordsRes = (query: string, range: Span, limit: number): Res<LogSearch> => ({
  key: `observe/logs/${spanKey(range)}/${limit}/${query}`,
  fetch: (signal) => observeApi.logs({ query, ...spanOf(range), limit }, signal),
  opts: { pollMs: pollOf(range) },
});

/**
 * A search for logs, not events (events are records of source `kiln.event`), unless it names
 * them: as a log monitor reads its search (kiln_obs::monitor::logs_only).
 */
export function logsOnly(q: string) {
  const s = q.trim();
  if (s.includes("kiln.event")) return s;
  return !s || s === "*" ? "-source:kiln.event" : `(${s}) -source:kiln.event`;
}

/** A search within a kind of record: the kind's own (`source:kiln.event` for events), and the search's. */
export function within(base: string, q: string) {
  const s = q.trim();
  const parts = [base, s && s !== "*" ? (base ? `(${s})` : s) : ""].filter(Boolean);
  return parts.join(" ") || "*";
}

/** A monitor's state as a tone (what the status dots and badges read), and as words. */
const STATE_TONE: Record<string, string> = { OK: "succeeded", WARN: "timed_out", ALERT: "failed", "NO DATA": "skipped" };
const STATE_WORDS: Record<string, string> = { OK: "OK", WARN: "Warn", ALERT: "Alert", "NO DATA": "No data" };

const CHECK_TONE: Record<CheckState, string> = { OK: "succeeded", WARNING: "timed_out", CRITICAL: "failed", UNKNOWN: "skipped" };
export const CHECK_WORDS: Record<CheckState, string> = { OK: "OK", WARNING: "Warning", CRITICAL: "Critical", UNKNOWN: "Unknown" };

/** A service check's status: its tone's dot and its word. */
export function CheckBadge({ status, className }: { status: CheckState; className?: string }) {
  const tone = CHECK_TONE[status];
  return (
    <span className={`status ${className ?? ""}`}>
      <i className="dot" data-status={tone} aria-hidden="true" />
      <span className="status-text" data-status={tone}>
        {CHECK_WORDS[status]}
      </span>
    </span>
  );
}

/** A monitor's (or a group's) state, and whether it is muted. */
export function StateBadge({ state, muted }: { state: string; muted?: boolean }) {
  const tone = STATE_TONE[state] ?? "skipped";
  return (
    <span className="status">
      <i className="dot" data-status={tone} aria-hidden="true" />
      <span className="status-text" data-status={tone}>
        {STATE_WORDS[state] ?? state}
        {muted && <span className="text-[#8c8f98]"> · muted</span>}
      </span>
    </span>
  );
}

/** A record's time: to the millisecond today, with its date before on another day. */
export function when(ns: number) {
  const d = new Date(ns / 1e6);
  const t = `${d.toLocaleTimeString(undefined, { hour12: false })}.${String(d.getMilliseconds()).padStart(3, "0")}`;
  return d.toDateString() === new Date().toDateString() ? t : `${d.toLocaleDateString(undefined, { month: "short", day: "numeric" })} ${t}`;
}

/** A range of times (ms) as read: its day once when both ends share it. */
export function rangeSaid(from: number, to: number): string {
  const day = (ms: number) => new Date(ms).toLocaleDateString(undefined, { month: "short", day: "numeric" });
  const time = (ms: number) => new Date(ms).toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit", hour12: false });
  return day(from) === day(to) ? `${day(from)}, ${time(from)} – ${time(to)}` : `${day(from)}, ${time(from)} – ${day(to)}, ${time(to)}`;
}

/** How a rehydration goes, in a few words. */
export function rehydrationSaid(r: Rehydration): string {
  const n = (x: number) => x.toLocaleString();
  switch (r.state) {
    case "queued":
      return "Waiting to be read";
    case "reading":
      return r.objects > 0 ? `Reading: ${n(r.objects_read)} of ${n(r.objects)} ${r.objects === 1 ? "object" : "objects"}` : "Listing its objects";
    case "failed":
      return `Failed: ${r.error ?? "no reason given"}`;
    default:
      return `${n(r.records)} ${r.records === 1 ? "record" : "records"}${r.limited ? ", stopped at its scan's bound" : ""}`;
  }
}

export const firstLine = (s: string) => {
  const i = s.indexOf("\n");
  return i < 0 ? s : s.slice(0, i);
};
