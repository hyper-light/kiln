/**
 * What every Observe page shares: its sections' tabs, the time range (in the URL, so a page is a
 * link), what a query answers over a range, and how it is drawn.
 */
import * as React from "react";
import { NavLink, useSearchParams } from "@/lib/router";
import { observeApi, type Cloud, type Dashboard, type DashboardSummary, type Integration, type LogHistogram, type LogSearch, type Monitor, type ObserveStatus, type QueryAnswer } from "@/api/observe";
import { Chart } from "@/components/charts/chart";
import { SERIES } from "@/components/charts/types";
import { Segmented, tabsTriggerClass } from "@/components/ui/tabs";
import { useRes, type Res } from "@/lib/data";
import { formatBytes, formatDuration } from "@/lib/format";

const SECTIONS = [
  { to: "/observe", label: "Overview", end: true },
  { to: "/observe/dashboards", label: "Dashboards" },
  { to: "/observe/metrics", label: "Metrics" },
  { to: "/observe/logs", label: "Logs" },
  { to: "/observe/events", label: "Events" },
  { to: "/observe/monitors", label: "Monitors" },
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

/** What `query` answers over the last `range`, read again as the range moves on. */
export const queryRes = (query: string, range: Range, points = 240): Res<QueryAnswer> => ({
  key: `observe/query/${range}/${points}/${query}`,
  fetch: (signal) => {
    const to = Date.now();
    return observeApi.query({ query, from: to - RANGES[range], to, points }, signal);
  },
  opts: { pollMs: pollFor(range) },
});

/** What several queries answer over one range, read together (a widget's, the explorer's). */
export const queriesRes = (queries: string[], range: Range, points = 240): Res<QueryAnswer[]> => ({
  key: `observe/queries/${range}/${points}/${queries.join("\n")}`,
  fetch: (signal) => {
    const to = Date.now();
    return Promise.all(queries.map((query) => observeApi.query({ query, from: to - RANGES[range], to, points }, signal)));
  },
  opts: { pollMs: pollFor(range) },
});

/** A log search's records over the range, by status, in about 60 buckets of a round size. */
export const histogramRes = (query: string, range: Range): Res<LogHistogram> => ({
  key: `observe/histogram/${range}/${query}`,
  fetch: (signal) => {
    const to = Date.now();
    return observeApi.histogram({ query, from: to - RANGES[range], to, step: niceStep(RANGES[range] / 60), by: ["status"] }, signal);
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
      return formatDuration(v);
    case "%":
      return `${compact(v)}%`;
    case "fraction":
      return `${compact(v * 100)}%`;
    default:
      return unit ? `${compact(v)} ${unit}` : compact(v);
  }
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

/** The time axis a range reads best in. */
const timeFormat = (r: Range) => (RANGES[r] > 2 * 86_400_000 ? "day" : "time");

/** Answers drawn as uPlot takes them: the first answer's buckets, and every series on them. */
export function chartData(answers: QueryAnswer[]): { data: (number | null)[][]; labels: string[] } {
  const first = answers[0];
  if (!first) return { data: [[]], labels: [] };
  const xs = Array.from({ length: first.grid.n }, (_, i) => (first.grid.from + i * first.grid.step) / 1000);
  const ys: (number | null)[][] = [];
  const labels: string[] = [];
  answers.forEach((a, qi) => {
    const same = a.grid.from === first.grid.from && a.grid.step === first.grid.step;
    for (const s of a.series) {
      labels.push(answers.length > 1 ? `${qi + 1}: ${seriesLabel(s.tags)}` : seriesLabel(s.tags));
      if (same) ys.push(s.values.slice(0, xs.length));
      else {
        // Another grid (its read came a bucket later): each bucket at its time on the first's.
        const at = new Map<number, number | null>();
        s.values.forEach((v, i) => at.set((a.grid.from + i * a.grid.step) / 1000, v));
        ys.push(xs.map((x) => at.get(x) ?? null));
      }
    }
  });
  return { data: [xs, ...ys], labels };
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
}: {
  queries: string[];
  range: Range;
  height?: number;
  unit?: string;
  display?: "line" | "bars" | "area";
  legend?: boolean;
  marks?: Mark[];
}) {
  const answers = useRes(queriesRes(queries, range)).data;
  const { data, labels } = React.useMemo(() => chartData(answers), [answers]);
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
      series={[
        ...labels.map((label, i) => ({ label, color: PALETTE[i % PALETTE.length] ?? SERIES.grey, bars: display === "bars", fill: display === "area" })),
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
export function Histogram({ h, range, height = 120 }: { h: LogHistogram; range: Range; height?: number }) {
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
// One item's key is not under its list's: a list refreshed after a delete must not refresh the item deleted.
export const monitorRes = (id: string): Res<Monitor> => ({ key: `observe/monitor/${id}`, fetch: (s) => observeApi.monitor(id, s), opts: { pollMs: 30_000 } });
export const integrationsRes: Res<Integration[]> = { key: "observe/integrations", fetch: (s) => observeApi.integrations(s) };
export const cloudsRes: Res<Cloud[]> = { key: "observe/clouds", fetch: (s) => observeApi.clouds(s), opts: { pollMs: 10_000 } };
export const dashboardsRes: Res<DashboardSummary[]> = { key: "observe/dashboards", fetch: (s) => observeApi.dashboards(s) };
export const dashboardRes = (id: string): Res<Dashboard> => ({ key: `observe/dashboard/${id}`, fetch: (s) => observeApi.dashboard(id, s) });
export const tagValuesRes = (tag: string): Res<string[]> => ({ key: `observe/tags/${tag}`, fetch: (s) => observeApi.tagValues(tag, s).then((r) => r.values), opts: { pollMs: 300_000 } });

/** The newest records a search finds over the range (a dashboard's log and event widgets). */
export const recordsRes = (query: string, range: Range, limit: number): Res<LogSearch> => ({
  key: `observe/logs/${range}/${limit}/${query}`,
  fetch: (signal) => {
    const to = Date.now();
    return observeApi.logs({ query, from: to - RANGES[range], to, limit }, signal);
  },
  opts: { pollMs: pollFor(range) },
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

export const firstLine = (s: string) => {
  const i = s.indexOf("\n");
  return i < 0 ? s : s.slice(0, i);
};
