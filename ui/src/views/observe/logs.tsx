/**
 * The logs explorer, and as events the event stream (events are records of source `kiln.event`):
 * a search (docs/observe.md §4.6), how its records fall over the range by status, each facet's
 * most common values (a click narrows the search to one, again widens it), and the newest
 * records, each opened to its attributes; or their patterns (Drain, §12.13), each a click from
 * its records; or, live, its records as they come (§6).
 */
import * as React from "react";
import { wsUrl } from "@/api/client";
import { observeApi, type LogFacets, type LogPatterns, type LogRecord, type LogTailMessage } from "@/api/observe";
import { SERIES } from "@/components/charts/types";
import { Button } from "@/components/ui/button";
import { Hold, RowsHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { PageIntro } from "@/components/ui/misc";
import { Segmented } from "@/components/ui/tabs";
import { useRes, type Res } from "@/lib/data";
import { Link } from "@/lib/router";
import { cn } from "@/lib/utils";
import {
  Histogram,
  ObserveNav,
  RANGES,
  RangePicker,
  STATUS_COLOR,
  STATUS_ORDER,
  firstLine,
  histogramRes,
  logsOnly,
  pollFor,
  recordsRes,
  useParam,
  useParams,
  useRange,
  when,
  within,
  type Range,
} from "./parts";

/** The newest records a page shows at first, each "more" adds, and shows at the most (the API's). */
const LIMIT = 100;
const MOST = 1000;
/** Records the live tail keeps on the page, the newest first; and the span its rates are over. */
const LIVE_KEEP = 500;
const RATE_WINDOW_MS = 5_000;

const KINDS = {
  logs: {
    title: "Logs",
    base: "",
    facets: ["service", "status", "host", "source"],
    lead: "Every log line kiln keeps: its own, its runners' and what OTLP exporters send. Search them, see how they fall over time, open any one.",
  },
  events: {
    title: "Events",
    base: "source:kiln.event",
    facets: ["service", "status", "@event.type", "@repo"],
    lead: "What happened: CI runs and jobs, deploys, monitors' alerts, GitHub's pushes and pull requests, and events posted to the API.",
  },
} as const;

const facetsRes = (query: string, range: Range, facets: readonly string[]): Res<LogFacets> => ({
  key: `observe/facets/${range}/${facets.join(",")}/${query}`,
  fetch: (signal) => {
    const to = Date.now();
    return observeApi.facets({ query, from: to - RANGES[range], to, facets: [...facets], top: 8 }, signal);
  },
  opts: { pollMs: pollFor(range) },
});

const patternsRes = (query: string, range: Range): Res<LogPatterns> => ({
  key: `observe/patterns/${range}/${query}`,
  fetch: (signal) => {
    const to = Date.now();
    return observeApi.patterns({ query, from: to - RANGES[range], to }, signal);
  },
  opts: { pollMs: pollFor(range) },
});

/** A template's fixed text, as search phrases: each token's pieces between its variables (eight at
 * the most), so its records are found whatever their spacing. */
function pieces(template: string): string[] {
  const out: string[] = [];
  for (const token of template.split(/\s+/)) for (const piece of token.split("<*>")) if (/[A-Za-z0-9]/.test(piece)) out.push(`"${piece.replace(/["\\]/g, "\\$&")}"`);
  return out.slice(0, 8);
}

/** `facet:value` as a search term, quoted when it must be. */
export function term(facet: string, value: string) {
  return /^[\w.\-/:@]+$/.test(value) ? `${facet}:${value}` : `${facet}:"${value.replace(/["\\]/g, "\\$&")}"`;
}

/** The search with `t` added, or taken out when it is there. */
export function toggle(q: string, t: string) {
  const words =
    q.trim() === "*"
      ? []
      : q
          .trim()
          .split(/\s+(?=(?:[^"]*"[^"]*")*[^"]*$)/)
          .filter(Boolean);
  return words.includes(t) ? words.filter((w) => w !== t).join(" ") : [...words, t].join(" ");
}

export function LogsExplorer({ kind }: { kind: keyof typeof KINDS }) {
  const k = KINDS[kind];
  const [q, setQ] = useParam("q");
  const [view, setView] = useParam("view", "records");
  const setParams = useParams();
  const [range, setRange] = useRange();
  const query = kind === "events" ? within(k.base, q) : logsOnly(q);
  return (
    <div className="container-console page">
      <title>{`${k.title} · Observe · kiln`}</title>
      <PageIntro
        index={8}
        label="Observe"
        title={k.title}
        lead={k.lead}
        actions={
          kind === "logs" ? (
            <div className="flex flex-wrap gap-2">
              <Button variant="secondary" asChild>
                <Link to="/observe/pipelines">Pipelines</Link>
              </Button>
              <Button variant="secondary" asChild>
                <Link to="/observe/log-metrics">Metrics from logs</Link>
              </Button>
              <Button variant="secondary" asChild>
                <Link to="/observe/archives">Archives</Link>
              </Button>
            </div>
          ) : undefined
        }
      />
      <ObserveNav />
      <SearchForm key={q} initial={q} onSearch={setQ} placeholder={kind === "events" ? "service:kiln.ci status:error" : 'service:api status:error "connection refused"'} />
      <div className="mb-5">
        <RangePicker value={range} onChange={setRange} />
      </div>
      <Loadable key={`h/${query}`} label="the histogram" fallback={<Hold height={146} />}>
        <HistogramOf query={query} range={range} />
      </Loadable>
      <div className="mt-6 grid grid-cols-1 gap-8 lg:grid-cols-[240px_minmax(0,1fr)]">
        <Loadable key={`f/${query}`} label="the facets" fallback={<RowsHold rows={8} rowHeight={32} />}>
          <Facets query={query} range={range} facets={k.facets} q={q} onToggle={(t) => setQ(toggle(q, t))} />
        </Loadable>
        <div className="min-w-0">
          <Segmented
            label="Show"
            className="mb-3"
            value={view === "patterns" || view === "live" ? view : "records"}
            onChange={(v) => setView(v)}
            options={[
              { value: "records", label: "Records" },
              { value: "patterns", label: "Patterns" },
              { value: "live", label: "Live tail" },
            ]}
          />
          {view === "live" ? (
            <Live key={query} query={query} onFilter={(t) => setQ(toggle(q, t))} />
          ) : view === "patterns" ? (
            <Loadable key={`p/${query}`} label="the patterns" fallback={<RowsHold rows={10} rowHeight={36} />}>
              <Patterns query={query} range={range} onPick={(terms) => setParams({ q: terms.reduce((acc, t) => (acc.split(/\s+/).includes(t) ? acc : toggle(acc, t)), q), view: null })} />
            </Loadable>
          ) : (
            <Loadable key={`r/${query}`} label="the records" fallback={<RowsHold rows={10} rowHeight={36} />}>
              <Records query={query} range={range} onFilter={(t) => setQ(toggle(q, t))} />
            </Loadable>
          )}
        </div>
      </div>
    </div>
  );
}

export function SearchForm({ initial, onSearch, placeholder }: { initial: string; onSearch: (q: string) => void; placeholder: string }) {
  const [draft, setDraft] = React.useState(initial);
  return (
    <form
      className="mb-4 flex flex-wrap items-end gap-3"
      onSubmit={(e) => {
        e.preventDefault();
        onSearch(draft.trim());
      }}
    >
      <div className="min-w-0 flex-1">
        <label htmlFor="observe-search" className="field-label">
          Search
        </label>
        <div className="field">
          <input
            id="observe-search"
            type="search"
            className="font-mono"
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
            placeholder={placeholder}
            spellCheck={false}
            autoComplete="off"
            autoCapitalize="off"
          />
        </div>
      </div>
      <Button type="submit">Search</Button>
    </form>
  );
}

function HistogramOf({ query, range }: { query: string; range: Range }) {
  const h = useRes(histogramRes(query, range)).data;
  return <Histogram h={h} range={range} height={120} />;
}

function Facets({ query, range, facets, q, onToggle }: { query: string; range: Range; facets: readonly string[]; q: string; onToggle: (t: string) => void }) {
  const f = useRes(facetsRes(query, range, facets)).data;
  return <FacetList f={f} q={q} onToggle={onToggle} noun={["record", "records"]} />;
}

/** Each facet's most common values, a click on one narrowing the search to it (again widening it). */
export function FacetList({ f, q, onToggle, noun }: { f: LogFacets; q: string; onToggle: (t: string) => void; noun: [string, string] }) {
  return (
    <aside aria-label="Facets" className="space-y-6">
      <p className="font-mono text-[10px] text-[#7b7e88]">
        {f.matched.toLocaleString()} {f.matched === 1 ? noun[0] : noun[1]} · {f.rows_scanned.toLocaleString()} read in {f.took_ms.toFixed(1)} ms
      </p>
      {f.facets.map((facet) => {
        const values = facet.values.filter((v) => v.value !== "N/A" && v.value !== "");
        if (values.length === 0) return null;
        const top = values[0]?.count ?? 1;
        return (
          <section key={facet.facet} aria-label={facet.facet}>
            <h3 className="field-label">
              {facet.facet}
              {facet.distinct > values.length && <span className="ml-1 text-[#6f7380]">· {facet.distinct.toLocaleString()}</span>}
            </h3>
            <ul className="space-y-0.5">
              {values.map((v) => {
                const t = term(facet.facet, v.value);
                const on = q.split(/\s+/).includes(t);
                return (
                  <li key={v.value}>
                    <button
                      type="button"
                      aria-pressed={on}
                      onClick={() => onToggle(t)}
                      className={cn(
                        "relative flex w-full items-center justify-between gap-2 overflow-hidden rounded px-2 py-1 text-left text-[12px] hover:text-white",
                        on ? "bg-white/[0.07] text-white" : "text-[#c8cad0] hover:bg-white/[0.05]",
                      )}
                    >
                      <span className="absolute inset-y-0 left-0 bg-white/[0.035]" style={{ width: `${(100 * v.count) / top}%` }} aria-hidden="true" />
                      <span className="relative flex min-w-0 items-center gap-2">
                        {facet.facet === "status" && <span className="inline-block size-2 shrink-0 rounded-full" style={{ background: STATUS_COLOR[v.value] }} aria-hidden="true" />}
                        <span className="truncate">{v.value}</span>
                      </span>
                      <span className="relative font-mono text-[10px] text-[#898d99] tabular-nums">{v.count.toLocaleString()}</span>
                    </button>
                  </li>
                );
              })}
            </ul>
          </section>
        );
      })}
    </aside>
  );
}

function Records({ query, range, onFilter }: { query: string; range: Range; onFilter: (t: string) => void }) {
  const [limit, setLimit] = React.useState(LIMIT);
  const found = useRes(recordsRes(query, range, limit)).data;
  const [open, setOpen] = React.useState<number | null>(null);
  if (found.records.length === 0) return <p className="empty-line py-16 text-center">Nothing matches in this range.</p>;
  return (
    <section aria-label="Records">
      <ol className="divide-y divide-[#1d1f23] border-y border-[#1d1f23]">
        {found.records.map((r, i) => (
          <RecordRow key={`${r.ts_ns}-${i}`} r={r} open={open === i} onToggle={() => setOpen(open === i ? null : i)} onFilter={onFilter} />
        ))}
      </ol>
      <div className="mt-3 flex flex-wrap items-center justify-between gap-3">
        <p className="font-mono text-[10px] text-[#7b7e88]">
          The newest {found.records.length.toLocaleString()} · {found.blocks.pruned.toLocaleString()} of {found.blocks.total.toLocaleString()} blocks ruled out by their summaries ·{" "}
          {found.took_ms.toFixed(1)} ms
        </p>
        {found.records.length >= limit && limit < MOST && (
          <Button variant="ghost" size="sm" onClick={() => React.startTransition(() => setLimit((l) => Math.min(MOST, l + LIMIT)))}>
            Show {LIMIT} more
          </Button>
        )}
      </div>
    </section>
  );
}

/** A record's row: its status, time, service and first line; opened, its message whole and its fields. */
function RecordRow({ r, open, onToggle, onFilter }: { r: LogRecord; open: boolean; onToggle: () => void; onFilter: (t: string) => void }) {
  return (
    <li>
      <button type="button" aria-expanded={open} onClick={onToggle} className="flex w-full items-baseline gap-3 px-2 py-2 text-left hover:bg-[#101114]">
        <span className="inline-block size-2 shrink-0 translate-y-[-1px] rounded-full" style={{ background: STATUS_COLOR[r.status] }} title={r.status} aria-label={r.status} />
        <time className="shrink-0 font-mono text-[10.5px] text-[#8c8f98] tabular-nums" dateTime={new Date(r.ts_ns / 1e6).toISOString()}>
          {when(r.ts_ns)}
        </time>
        {r.service && <span className="hidden shrink-0 font-mono text-[10.5px] text-[#a7aab3] sm:inline">{r.service}</span>}
        <span className={cn("min-w-0 flex-1 text-[12.5px] text-[#d6d8dd]", open ? "whitespace-pre-wrap break-words" : "truncate")}>{open ? r.message : firstLine(r.message)}</span>
      </button>
      {open && <Detail r={r} onFilter={onFilter} />}
    </li>
  );
}

/** A rate a second, as read: tenths below ten. */
function perSecond(n: number) {
  return n < 10 ? n.toFixed(1).replace(/\.0$/, "") : Math.round(n).toLocaleString();
}

type Shown = { seq: number; r: LogRecord };
type TailState = { kind: "opening" | "live" | "ended"; why?: string };

/** The search's records as they come, the newest first: every one while they are few, a uniformly
 * random sample once they come faster than they can be read, how fast they match said either way.
 * Paused, or a record opened, what comes waits (counted) until resumed. */
function Live({ query, onFilter }: { query: string; onFilter: (t: string) => void }) {
  const [shown, setShown] = React.useState<Shown[]>([]);
  const [state, setState] = React.useState<TailState>({ kind: "opening" });
  const [rate, setRate] = React.useState({ matched: 0, shown: 0 });
  const [paused, setPausedState] = React.useState(false);
  const [waiting, setWaiting] = React.useState(0);
  const [open, setOpen] = React.useState<number | null>(null);
  const [epoch, setEpoch] = React.useState(0);
  const pausedRef = React.useRef(false);
  const heldRef = React.useRef<Shown[]>([]);
  const seqRef = React.useRef(0);
  const path = observeApi.tailPath(query);
  const setPaused = (p: boolean) => {
    pausedRef.current = p;
    setPausedState(p);
    if (!p) {
      const held = heldRef.current;
      heldRef.current = [];
      setWaiting(0);
      setShown((s) => [...held, ...s].slice(0, LIVE_KEEP));
    }
  };
  const stream = React.useCallback(
    (el: HTMLElement | null) => {
      if (!el) return;
      const ticks: { at: number; matched: number; shown: number }[] = [];
      const ws = new WebSocket(wsUrl(path));
      ws.onopen = () => setState({ kind: "live" });
      ws.onmessage = (m) => {
        if (typeof m.data !== "string") return;
        const msg = JSON.parse(m.data) as LogTailMessage;
        if (msg.type === "error") {
          setState({ kind: "ended", why: msg.message });
          return;
        }
        const now = Date.now();
        ticks.push({ at: now, matched: msg.matched, shown: msg.records.length });
        while ((ticks.at(0)?.at ?? now) < now - RATE_WINDOW_MS) ticks.shift();
        const span = (Math.max(0, now - (ticks.at(0)?.at ?? now)) + msg.ms) / 1000;
        setRate({ matched: ticks.reduce((a, t) => a + t.matched, 0) / span, shown: ticks.reduce((a, t) => a + t.shown, 0) / span });
        if (msg.records.length === 0) return;
        const add = msg.records.map((r) => ({ seq: seqRef.current++, r })).reverse();
        if (pausedRef.current) {
          heldRef.current = [...add, ...heldRef.current].slice(0, LIVE_KEEP);
          setWaiting((w) => w + add.length);
        } else setShown((s) => [...add, ...s].slice(0, LIVE_KEEP));
      };
      ws.onclose = () => setState((s) => (s.kind === "ended" ? s : { kind: "ended" }));
      return () => ws.close();
    },
    // A new socket for each "follow again".
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [path, epoch],
  );
  const sampled = rate.matched > rate.shown * 1.05 + 0.5;
  const said =
    state.kind === "opening"
      ? "Opening the tail…"
      : state.kind === "ended"
        ? state.why
          ? `Stopped: ${state.why}`
          : "Stopped."
        : paused
          ? `Paused · ${waiting.toLocaleString()} new`
          : sampled
            ? `Live · ${perSecond(rate.matched)} a second · showing a sample of ${perSecond(rate.shown)} a second`
            : `Live · ${perSecond(rate.matched)} a second`;
  return (
    <section aria-label="Live tail" ref={stream}>
      <div className="mb-2 flex items-start justify-between gap-3">
        <p className="flex min-w-0 items-baseline gap-2 pt-1.5 font-mono text-[10.5px] text-[#8c8f98]" aria-live="polite">
          <span className="inline-block size-1.5 shrink-0 -translate-y-px rounded-full" style={{ background: state.kind === "live" && !paused ? SERIES.ok : "#5b5e66" }} aria-hidden="true" />
          <span className="min-w-0">{said}</span>
        </p>
        {state.kind === "ended" ? (
          <Button
            variant="ghost"
            size="sm"
            className="shrink-0"
            onClick={() => {
              setState({ kind: "opening" });
              setEpoch((e) => e + 1);
            }}
          >
            Follow again
          </Button>
        ) : (
          <Button variant="ghost" size="sm" className="shrink-0" onClick={() => setPaused(!paused)}>
            {paused ? "Resume" : "Pause"}
          </Button>
        )}
      </div>
      {shown.length === 0 ? (
        <p className="empty-line py-16 text-center">{state.kind === "ended" ? "Nothing came." : "Records that match show here as they come."}</p>
      ) : (
        <ol className="divide-y divide-[#1d1f23] border-y border-[#1d1f23]">
          {shown.map(({ seq, r }) => (
            <RecordRow
              key={seq}
              r={r}
              open={open === seq}
              onToggle={() => {
                if (open === seq) setOpen(null);
                else {
                  setOpen(seq);
                  if (!paused) setPaused(true);
                }
              }}
              onFilter={onFilter}
            />
          ))}
        </ol>
      )}
    </section>
  );
}

/** A template, its variables quiet. */
function Template({ text }: { text: string }) {
  const parts = text.split("<*>");
  return (
    <>
      {parts.map((s, i) => (
        <React.Fragment key={i}>
          {s}
          {i < parts.length - 1 && <span className="text-[#6f7380]">*</span>}
        </React.Fragment>
      ))}
    </>
  );
}

/** A search's patterns, the most common first: each its count, its worst status, its template, when
 * last seen; a click shows its records. */
function Patterns({ query, range, onPick }: { query: string; range: Range; onPick: (terms: string[]) => void }) {
  const p = useRes(patternsRes(query, range)).data;
  if (p.patterns.length === 0) return <p className="empty-line py-16 text-center">Nothing matches in this range.</p>;
  return (
    <section aria-label="Patterns">
      <ol className="divide-y divide-[#1d1f23] border-y border-[#1d1f23]">
        {p.patterns.map((x) => {
          const worst = STATUS_ORDER.filter((s) => x.statuses[s]).at(-1) ?? "info";
          const said = Object.entries(x.statuses)
            .map(([s, n]) => `${s} ${n.toLocaleString()}`)
            .join(", ");
          return (
            <li key={x.template}>
              <button type="button" onClick={() => onPick(pieces(x.template))} title={x.sample} className="flex w-full items-baseline gap-3 px-2 py-2 text-left hover:bg-[#101114]">
                <span className="w-12 shrink-0 text-right font-mono text-[11px] text-[#c8cad0] tabular-nums">{x.count.toLocaleString()}</span>
                <span className="inline-block size-2 shrink-0 translate-y-[-1px] rounded-full" style={{ background: STATUS_COLOR[worst] }} title={said} aria-label={said} />
                <span className="min-w-0 flex-1 truncate font-mono text-[12px] text-[#d6d8dd]">
                  <Template text={x.template} />
                </span>
                <time className="hidden shrink-0 font-mono text-[10.5px] text-[#8c8f98] tabular-nums sm:inline" dateTime={new Date(x.last_ns / 1e6).toISOString()}>
                  {when(x.last_ns)}
                </time>
              </button>
            </li>
          );
        })}
      </ol>
      <p className="mt-3 font-mono text-[10px] text-[#7b7e88]">
        {p.patterns.length.toLocaleString()} {p.patterns.length === 1 ? "pattern" : "patterns"} in the newest {p.records.toLocaleString()} records · {p.took_ms.toFixed(1)} ms
      </p>
    </section>
  );
}

/** A record's fields and attributes; each one a filter a click away. */
function Detail({ r, onFilter }: { r: LogRecord; onFilter: (t: string) => void }) {
  const fields: [string, string, string][] = [
    ["host", r.host, "host"],
    ["service", r.service, "service"],
    ["source", r.source, "source"],
    ["status", r.status, "status"],
    ...Object.entries(r.attributes).map(([k, v]): [string, string, string] => [`@${k}`, v, `@${k}`]),
  ];
  // Its trace's page, a click away.
  if (r.trace_id) fields.push(["trace", r.trace_id, "trace"]);
  return (
    <div className="mb-3 ml-5 overflow-x-auto px-2 pb-2">
      <p className="mb-2 font-mono text-[10px] text-[#7b7e88]">{new Date(r.ts_ns / 1e6).toISOString()}</p>
      <table className="w-full text-[12px]">
        <tbody>
          {fields
            .filter(([, v]) => v)
            .map(([k, v, facet]) => (
              <tr key={k}>
                <th scope="row" className="w-1 py-0.5 pr-4 text-left align-top font-mono text-[10.5px] font-normal whitespace-nowrap text-[#8c8f98]">
                  {k}
                </th>
                <td className="py-0.5 align-top break-all text-[#c8cad0]">
                  {facet === "trace" ? (
                    <Link to={`/observe/traces/${v}`} className="font-mono text-[11px] hover:text-white hover:underline" title="Its trace">
                      {v}
                    </Link>
                  ) : facet ? (
                    <button type="button" className="text-left hover:text-white hover:underline" title={`Search for ${k}:${v}`} onClick={() => onFilter(term(facet, v))}>
                      {v}
                    </button>
                  ) : (
                    <span className="font-mono text-[11px]">{v}</span>
                  )}
                </td>
              </tr>
            ))}
        </tbody>
      </table>
    </div>
  );
}

export default function Logs() {
  return <LogsExplorer kind="logs" />;
}
