/**
 * Traces (docs/observe.md §8.7): the trace explorer, spans searched as Datadog's trace explorer
 * searches them (`service:checkout resource_name:"GET /cart" status:error @duration:>250ms`), how
 * they fall over the range by status, each facet's most common values, the newest spans; and a
 * trace, its spans as a waterfall draws them (each under its parent, as long as it took, where it
 * began), the one picked opened to its attributes, its error and its events, and the logs of the
 * trace beside it.
 */
import * as React from "react";
import { observeApi, type LogFacets, type LogHistogram, type LogSearch, type PlacedSpan, type SpanSearch, type Trace } from "@/api/observe";
import { SERIES } from "@/components/charts/types";
import { Button } from "@/components/ui/button";
import { Hold, RowsHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { Crumbs, Facts, PageIntro, Panel } from "@/components/ui/misc";
import { useRes, type Res } from "@/lib/data";
import { Link, useParams } from "@/lib/router";
import { cn } from "@/lib/utils";
import { ApmTabs, latency } from "./apm";
import { FacetList, SearchForm, toggle } from "./logs";
import { Histogram, ObserveNav, PALETTE, RANGES, RangePicker, STATUS_COLOR, firstLine, niceStep, pollFor, useParam, useRange, when, type Range } from "./parts";

/** The newest spans a page shows. */
const LIMIT = 200;
const FACETS = ["service", "resource_name", "status", "@span.kind", "@env", "@http.status_code", "@peer.service"] as const;

const spansRes = (query: string, range: Range): Res<SpanSearch> => ({
  key: `observe/spans/${range}/${query}`,
  fetch: (signal) => {
    const to = Date.now();
    return observeApi.spans({ query, from: to - RANGES[range], to, limit: LIMIT }, signal);
  },
  opts: { pollMs: pollFor(range) },
});

const spanHistogramRes = (query: string, range: Range): Res<LogHistogram> => ({
  key: `observe/span-histogram/${range}/${query}`,
  fetch: (signal) => {
    const to = Date.now();
    return observeApi.spanHistogram({ query, from: to - RANGES[range], to, step: niceStep(RANGES[range] / 60), by: ["status"] }, signal);
  },
  opts: { pollMs: pollFor(range) },
});

const spanFacetsRes = (query: string, range: Range): Res<LogFacets> => ({
  key: `observe/span-facets/${range}/${query}`,
  fetch: (signal) => {
    const to = Date.now();
    return observeApi.spanFacets({ query, from: to - RANGES[range], to, facets: [...FACETS], top: 8 }, signal);
  },
  opts: { pollMs: pollFor(range) },
});

const traceRes = (id: string): Res<Trace> => ({ key: `observe/trace/${id}`, fetch: (s) => observeApi.trace(id, s) });

/** A span's duration (ns, as its `duration` says) as read. */
const took = (ns: number | string | undefined) => latency(ns === undefined ? null : Number(ns) / 1e9);

export default function TraceExplorer() {
  const [q, setQ] = useParam("q");
  const [range, setRange] = useRange("15m");
  const query = q.trim() || "*";
  return (
    <div className="container-console page">
      <title>Traces · Observe · kiln</title>
      <PageIntro index={8} label="Observe" title="Traces" lead="Every span services send: search them as Datadog's trace explorer does, see how they fall over time, and open any one's trace." />
      <ObserveNav />
      <ApmTabs />
      <SearchForm key={q} initial={q} onSearch={setQ} placeholder={'service:checkout status:error @duration:>250ms resource_name:"GET /cart"'} />
      <div className="mb-5">
        <RangePicker value={range} onChange={setRange} />
      </div>
      <Loadable key={`h/${query}`} label="the histogram" fallback={<Hold height={146} />}>
        <SpanHistogram query={query} range={range} />
      </Loadable>
      <div className="mt-6 grid grid-cols-1 gap-8 lg:grid-cols-[240px_minmax(0,1fr)]">
        <Loadable key={`f/${query}`} label="the facets" fallback={<RowsHold rows={8} rowHeight={32} />}>
          <SpanFacets query={query} range={range} q={q} onToggle={(t) => setQ(toggle(q, t))} />
        </Loadable>
        <Loadable key={`s/${query}`} label="the spans" fallback={<RowsHold rows={10} rowHeight={36} />}>
          <Spans query={query} range={range} />
        </Loadable>
      </div>
    </div>
  );
}

function SpanHistogram({ query, range }: { query: string; range: Range }) {
  const h = useRes(spanHistogramRes(query, range)).data;
  return <Histogram h={h} range={range} height={120} />;
}

function SpanFacets({ query, range, q, onToggle }: { query: string; range: Range; q: string; onToggle: (t: string) => void }) {
  const f = useRes(spanFacetsRes(query, range)).data;
  return <FacetList f={f} q={q} onToggle={onToggle} noun={["span", "spans"]} />;
}

function Spans({ query, range }: { query: string; range: Range }) {
  const found = useRes(spansRes(query, range)).data;
  if (found.spans.length === 0) return <p className="empty-line py-16 text-center">No spans match in this range.</p>;
  return (
    <section aria-label="Spans">
      <ol className="divide-y divide-[#1d1f23] border-y border-[#1d1f23]">
        {found.spans.map((s, i) => (
          <li key={`${s.span_id ?? i}`}>
            <Link
              to={s.trace_id ? `/observe/traces/${s.trace_id}${s.span_id ? `?span=${s.span_id}` : ""}` : "/observe/traces"}
              className="flex w-full items-baseline gap-3 px-2 py-2 text-left hover:bg-[#101114]"
            >
              <span className="inline-block size-2 shrink-0 translate-y-[-1px] rounded-full" style={{ background: STATUS_COLOR[s.status] }} title={s.status} aria-label={s.status} />
              <time className="shrink-0 font-mono text-[10.5px] text-[#8c8f98] tabular-nums" dateTime={new Date(s.ts_ns / 1e6).toISOString()}>
                {when(s.ts_ns)}
              </time>
              <span className="hidden w-32 shrink-0 truncate font-mono text-[10.5px] text-[#a7aab3] sm:inline">{s.service}</span>
              <span className="min-w-0 flex-1 truncate text-[12.5px] text-[#d6d8dd]">{firstLine(s.message)}</span>
              <span className="hidden shrink-0 font-mono text-[10.5px] text-[#8c8f98] md:inline">{s.attributes["span.kind"]}</span>
              <span className="w-16 shrink-0 text-right font-mono text-[11px] text-[#c8cad0] tabular-nums">{took(s.attributes.duration)}</span>
            </Link>
          </li>
        ))}
      </ol>
      <p className="mt-3 font-mono text-[10px] text-[#7b7e88]">
        The newest {found.spans.length.toLocaleString()} · {found.blocks.pruned.toLocaleString()} of {found.blocks.total.toLocaleString()} blocks ruled out by their summaries ·{" "}
        {found.took_ms.toFixed(1)} ms
      </p>
    </section>
  );
}

export function TracePage() {
  const { id = "" } = useParams();
  return (
    <div className="container-console page">
      <title>{`Trace ${id.slice(0, 8)} · Observe · kiln`}</title>
      <header className="page-intro">
        <div className="min-w-0">
          <Crumbs index={8} items={[{ label: "Observe", to: "/observe" }, { label: "Traces", to: "/observe/traces" }, { label: id.slice(0, 16) }]} />
        </div>
      </header>
      <Loadable key={id} label="the trace" fallback={<Hold height={480} label="Loading the trace" />}>
        <TraceView id={id} />
      </Loadable>
    </div>
  );
}

/** Each service's color, by its place among the trace's (sorted by name): the same on every row. */
function serviceColors(t: Trace): Map<string, string> {
  return new Map(t.services.map((s, i) => [s.service, PALETTE[i % PALETTE.length] ?? SERIES.grey]));
}

function TraceView({ id }: { id: string }) {
  const t = useRes(traceRes(id)).data;
  const [spanParam, setSpanParam] = useParam("span");
  const colors = React.useMemo(() => serviceColors(t), [t]);
  const picked = Math.max(
    0,
    t.spans.findIndex((s) => s.span_id === spanParam),
  );
  const span = t.spans[picked];
  return (
    <div className="space-y-6">
      <div>
        <h1 className="truncate text-[20px] text-[#f2f2f3]">
          <span className="font-mono text-[#a7aab3]">{t.root?.service}</span> {t.root?.resource_name}
        </h1>
        <Facts
          className="mt-3"
          items={[
            { label: "Duration", value: took(t.duration_ns) },
            { label: "Spans", value: t.truncated ? `${t.spans.length.toLocaleString()} of ${t.spans_count.toLocaleString()}` : t.spans_count.toLocaleString() },
            { label: "Errors", value: t.errors.toLocaleString(), tone: t.errors > 0 ? "fail" : undefined },
            { label: "Started", value: when(t.start_ns) },
          ]}
        />
        <ul className="mt-3 flex flex-wrap gap-x-4 gap-y-1" aria-label="Services">
          {t.services.map((s) => (
            <li key={s.service} className="flex items-center gap-1.5 text-[12px] text-[#c8cad0]">
              <span className="inline-block size-2 rounded-sm" style={{ background: colors.get(s.service) }} aria-hidden="true" />
              <Link to={`/observe/apm/services/${encodeURIComponent(s.service)}`} className="font-mono hover:text-white">
                {s.service || "(no service)"}
              </Link>
              <span className="text-[#8c8f98]">{s.spans}</span>
            </li>
          ))}
        </ul>
        {t.roots > 1 && <p className="mt-2 text-[12px] text-[#8c8f98]">{`${t.roots} spans' parents are not held (not sent, or past retention): each starts a tree of its own.`}</p>}
      </div>
      <Waterfall t={t} colors={colors} picked={picked} onPick={(i) => setSpanParam(t.spans[i]?.span_id ?? "")} />
      {span && <SpanDetail span={span} color={colors.get(span.service)} />}
      <Panel title="Logs of this trace" description="Log records that carry its trace id, from a minute before it to a minute after.">
        <Loadable label="the logs" fallback={<RowsHold rows={3} rowHeight={32} />}>
          <TraceLogs t={t} />
        </Loadable>
      </Panel>
    </div>
  );
}

/** Rows a waterfall draws before it asks to show the rest: a trace's first screens. */
const ROWS = 400;

function Waterfall({ t, colors, picked, onPick }: { t: Trace; colors: Map<string, string>; picked: number; onPick: (i: number) => void }) {
  const [all, setAll] = React.useState(false);
  const total = Math.max(1, t.duration_ns);
  const rows = all ? t.spans : t.spans.slice(0, ROWS);
  const ticks = [0, 0.25, 0.5, 0.75, 1];
  return (
    <section aria-label="Waterfall" className="rounded-[12px] border border-[#1d1f23]">
      <div className="grid grid-cols-1 border-b border-[#1d1f23] px-3 py-2 sm:grid-cols-[minmax(200px,38%)_minmax(0,1fr)]">
        <span className="hidden font-mono text-[10px] text-[#7b7e88] sm:block">Span</span>
        <div className="relative h-4 font-mono text-[10px] text-[#7b7e88]" aria-hidden="true">
          {ticks.map((f) => (
            <span key={f} className="absolute top-0 whitespace-nowrap" style={f === 1 ? { right: 0 } : { left: `${f * 100}%` }}>
              {took(total * f)}
            </span>
          ))}
        </div>
      </div>
      <ol>
        {rows.map((s, i) => (
          <WaterfallRow key={`${s.span_id ?? i}`} s={s} total={total} color={colors.get(s.service) ?? SERIES.grey} picked={i === picked} onPick={() => onPick(i)} />
        ))}
      </ol>
      {!all && t.spans.length > ROWS && (
        <div className="border-t border-[#1d1f23] px-3 py-2">
          <Button variant="ghost" size="sm" onClick={() => React.startTransition(() => setAll(true))}>
            Show all {t.spans.length.toLocaleString()} spans
          </Button>
        </div>
      )}
    </section>
  );
}

function WaterfallRow({ s, total, color, picked, onPick }: { s: PlacedSpan; total: number; color: string; picked: boolean; onPick: () => void }) {
  const left = (100 * s.offset_ns) / total;
  const width = Math.max((100 * s.duration_ns) / total, 0.15);
  const error = s.status === "error";
  return (
    <li>
      <button
        type="button"
        aria-pressed={picked}
        onClick={onPick}
        className={cn("grid w-full grid-cols-1 items-center gap-1 px-3 py-1.5 text-left sm:grid-cols-[minmax(200px,38%)_minmax(0,1fr)] sm:gap-3", picked ? "bg-white/[0.06]" : "hover:bg-[#101114]")}
      >
        <span className="flex min-w-0 items-center gap-2" style={{ paddingLeft: Math.min(s.depth, 12) * 12 }}>
          <span className="inline-block h-3 w-1 shrink-0 rounded-sm" style={{ background: color }} aria-hidden="true" />
          {error && <span className="inline-block size-1.5 shrink-0 rounded-full" style={{ background: STATUS_COLOR.error }} aria-label="error" />}
          <span className="shrink-0 font-mono text-[10.5px] text-[#a7aab3]">{s.service}</span>
          <span className="min-w-0 truncate text-[12px] text-[#d6d8dd]">{s.resource_name}</span>
          <span className="ml-auto shrink-0 font-mono text-[10.5px] text-[#8c8f98] tabular-nums sm:hidden">{took(s.duration_ns)}</span>
        </span>
        <span className="relative block h-4">
          <span
            className="absolute top-0.5 h-3 rounded-[3px]"
            style={{
              left: `${Math.min(left, 99.85)}%`,
              width: `${Math.min(width, 100 - Math.min(left, 99.85))}%`,
              background: color,
              opacity: picked ? 1 : 0.75,
              boxShadow: error ? `inset 0 0 0 1.5px ${STATUS_COLOR.error}` : undefined,
            }}
            aria-hidden="true"
          />
          <span
            className="absolute top-0 hidden font-mono text-[10px] whitespace-nowrap text-[#c8cad0] sm:inline"
            style={left + width > 80 ? { right: `${100 - left + 0.5}%` } : { left: `${left + width + 0.5}%` }}
          >
            {took(s.duration_ns)}
          </span>
        </span>
      </button>
    </li>
  );
}

/** What a span's attributes hold that the detail shows apart. */
const APART = new Set(["events", "links", "error.type", "error.message", "error.stack", "duration", "parent_id"]);

interface SpanEvent {
  name: string;
  offset_ns: number;
  attributes: Record<string, string>;
}

function parse<T>(text: string | undefined): T[] {
  if (!text) return [];
  try {
    const v = JSON.parse(text) as unknown;
    return Array.isArray(v) ? (v as T[]) : [];
  } catch {
    return [];
  }
}

function SpanDetail({ span, color }: { span: PlacedSpan; color?: string }) {
  const a = span.attributes;
  const events = parse<SpanEvent>(a.events);
  const links = parse<{ trace_id: string; span_id: string; attributes: Record<string, string> }>(a.links);
  const error = a["error.type"] || a["error.message"] || a["error.stack"];
  const rows = Object.entries(a).filter(([k]) => !APART.has(k));
  return (
    <Panel
      title={
        <span className="flex min-w-0 items-center gap-2">
          <span className="inline-block h-3 w-1 shrink-0 rounded-sm" style={{ background: color }} aria-hidden="true" />
          <span className="truncate">{span.resource_name}</span>
        </span>
      }
      description={`${span.service} · ${a["span.kind"] ?? "internal"} · ${took(span.duration_ns)}, from ${took(span.offset_ns)} into the trace`}
    >
      {error && (
        <div className="mb-4 rounded-[8px] border border-[#f27a7a]/30 bg-[#f27a7a]/[0.06] p-3">
          <p className="text-[12.5px] text-[#f2b0b0]">
            {a["error.type"] && <span className="font-mono">{a["error.type"]}: </span>}
            {a["error.message"]}
          </p>
          {a["error.stack"] && <pre className="mt-2 max-h-64 overflow-auto font-mono text-[11px] whitespace-pre-wrap text-[#c8cad0]">{a["error.stack"]}</pre>}
        </div>
      )}
      <div className="overflow-x-auto">
        <table className="w-full text-[12px]">
          <tbody>
            {[["span_id", span.span_id ?? ""], ["host", span.host], ["source", span.source], ...rows].map(([k, v]) => (
              <tr key={k}>
                <th scope="row" className="w-1 py-0.5 pr-4 text-left align-top font-mono text-[10.5px] font-normal whitespace-nowrap text-[#8c8f98]">
                  {k}
                </th>
                <td className="py-0.5 align-top break-all text-[#c8cad0]">{v}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {events.length > 0 && (
        <section aria-label="Events" className="mt-4">
          <h3 className="field-label">Events</h3>
          <ol className="space-y-2">
            {events.map((e, i) => (
              <li key={i} className="text-[12px] text-[#c8cad0]">
                <span className="font-mono text-[#8c8f98]">+{took(e.offset_ns)}</span> {e.name}
                {Object.keys(e.attributes ?? {}).length > 0 && (
                  <span className="block truncate pl-4 font-mono text-[11px] text-[#8c8f98]">
                    {Object.entries(e.attributes)
                      .filter(([k]) => k !== "exception.stacktrace")
                      .map(([k, v]) => `${k}=${v}`)
                      .join("  ")}
                  </span>
                )}
              </li>
            ))}
          </ol>
        </section>
      )}
      {links.length > 0 && (
        <section aria-label="Links" className="mt-4">
          <h3 className="field-label">Links</h3>
          <ul className="space-y-1">
            {links.map((l) => (
              <li key={`${l.trace_id}/${l.span_id}`} className="font-mono text-[12px]">
                <Link to={`/observe/traces/${l.trace_id}?span=${l.span_id}`} className="text-[#c8cad0] hover:text-white hover:underline">
                  {l.trace_id.slice(0, 16)}…/{l.span_id}
                </Link>
              </li>
            ))}
          </ul>
        </section>
      )}
    </Panel>
  );
}

const traceLogsRes = (t: Trace): Res<LogSearch> => ({
  key: `observe/trace-logs/${t.trace_id}`,
  fetch: (signal) => observeApi.logs({ query: `trace_id:${t.trace_id}`, from: Math.floor(t.start_ns / 1e6) - 60_000, to: Math.ceil(t.end_ns / 1e6) + 60_000, limit: 100 }, signal),
});

function TraceLogs({ t }: { t: Trace }) {
  const found = useRes(traceLogsRes(t)).data;
  if (found.records.length === 0) return <p className="empty-line py-6 text-center">No log records carry this trace's id.</p>;
  return (
    <>
      <ol className="divide-y divide-[#1d1f23] border-y border-[#1d1f23]">
        {[...found.records].reverse().map((r, i) => (
          <li key={`${r.ts_ns}-${i}`} className="flex items-baseline gap-3 px-2 py-1.5">
            <span className="inline-block size-2 shrink-0 translate-y-[-1px] rounded-full" style={{ background: STATUS_COLOR[r.status] }} title={r.status} aria-label={r.status} />
            <span className="shrink-0 font-mono text-[10.5px] text-[#8c8f98] tabular-nums">+{took(r.ts_ns - t.start_ns)}</span>
            <span className="hidden shrink-0 font-mono text-[10.5px] text-[#a7aab3] sm:inline">{r.service}</span>
            <span className="min-w-0 flex-1 truncate text-[12.5px] text-[#d6d8dd]">{firstLine(r.message)}</span>
          </li>
        ))}
      </ol>
      <p className="mt-2 text-[12px]">
        <Link to={`/observe/logs?q=${encodeURIComponent(`trace_id:${t.trace_id}`)}&range=1d`} className="text-[#c8cad0] hover:text-white hover:underline">
          In the logs explorer
        </Link>
      </p>
    </>
  );
}
