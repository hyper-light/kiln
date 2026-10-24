/**
 * LLM Observability (docs/observe.md §8.12): an app's calls to language models and its agents' and
 * tools' steps, read from the spans it sends in any convention (OpenTelemetry's GenAI conventions,
 * OpenInference, OpenLLMetry, Datadog's LLM Observability, Claude Code's own): what they cost, their
 * tokens, errors and latency over the range, by model; its newest LLM traces; and a trace's steps as
 * a tree, the one picked opened to its messages, its tokens and its cost.
 */
import * as React from "react";
import { observeApi, type LogSearch, type PlacedSpan, type Span, type SpanAggregate, type SpanSearch, type Trace } from "@/api/observe";
import { Hold, RowsHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { Crumbs, Facts, PageIntro, Panel } from "@/components/ui/misc";
import { useRes, type Res } from "@/lib/data";
import { Link, useParams } from "@/lib/router";
import { cn } from "@/lib/utils";
import { latency } from "./apm";
import { ObserveNav, QueryChart, RANGES, RangePicker, STATUS_COLOR, compact, formatUsd, pollFor, useParam, useRange, when, type Range } from "./parts";

/** A model's call: an LLM's or an embedding's. */
const CALLS = "(@meta.span.kind:llm OR @meta.span.kind:embedding)";
/** The LLM traces a page lists, and the newest LLM steps it reads to find them (a trace's steps
 * under a request's span have no LLM root to search for: each trace is its steps, grouped). */
const TRACES = 50;
const STEPS = 1000;

const quoted = (v: string) => `"${v.replace(/["\\]/g, "\\$&")}"`;
const appQuery = (app: string) => (app ? ` @ml_app:${quoted(app)}` : "");
/** A metric query's filter: the app's, and `more`; `*` where it says none. */
const tagsOf = (app: string, ...more: string[]) => [app ? `ml_app:${app}` : "", ...more].filter(Boolean).join(",") || "*";

/** What a model was asked over the range: its calls, tokens, cost, errors and time, each a sum or a mean of its spans. */
interface ModelRow {
  model: string;
  provider: string;
  calls: number;
  input: number;
  output: number;
  cost: number;
  errors: number;
  avgNs: number;
}

interface Overview {
  apps: { app: string; spans: number }[];
  models: ModelRow[];
}

const key = (g: Record<string, string>) => `${g["@meta.model_name"] ?? ""}\u0000${g["@meta.model_provider"] ?? ""}`;

const overviewRes = (app: string, range: Range): Res<Overview> => ({
  key: `observe/llm/${range}/${app}`,
  fetch: async (signal) => {
    const to = Date.now();
    const from = to - RANGES[range];
    const by = ["@meta.model_name", "@meta.model_provider"];
    const q = CALLS + appQuery(app);
    const agg = (query: string, measure?: "sum" | "avg", of?: string) => observeApi.spanAggregate({ query, from, to, by, measure, of }, signal);
    const [apps, calls, input, output, cost, errors, avg] = await Promise.all([
      observeApi.spanAggregate({ query: "@meta.span.kind:*", from, to, by: ["@ml_app"] }, signal),
      agg(q),
      agg(q, "sum", "@metrics.input_tokens"),
      agg(q, "sum", "@metrics.output_tokens"),
      agg(q, "sum", "@metrics.estimated_total_cost"),
      agg(`${q} status:error`),
      agg(q, "avg", "@duration"),
    ]);
    const of = (a: SpanAggregate) => new Map(a.groups.map((g) => [key(g.group), g.value ?? 0]));
    const [i, o, c, e, d] = [of(input), of(output), of(cost), of(errors), of(avg)];
    const models = calls.groups
      .map((g) => {
        const k = key(g.group);
        return {
          model: g.group["@meta.model_name"] ?? "",
          provider: g.group["@meta.model_provider"] ?? "",
          calls: g.value ?? 0,
          input: i.get(k) ?? 0,
          output: o.get(k) ?? 0,
          cost: c.get(k) ?? 0,
          errors: e.get(k) ?? 0,
          avgNs: d.get(k) ?? 0,
        };
      })
      .sort((a, b) => b.cost - a.cost || b.calls - a.calls);
    return { apps: apps.groups.map((g) => ({ app: g.group["@ml_app"] ?? "", spans: g.value ?? 0 })).sort((a, b) => b.spans - a.spans), models };
  },
  opts: { pollMs: pollFor(range) },
});

const sessionQuery = (session: string) => (session ? ` @session_id:${quoted(session)}` : "");

const tracesRes = (app: string, session: string, range: Range): Res<SpanSearch> => ({
  key: `observe/llm/traces/${range}/${app}/${session}`,
  fetch: (signal) => {
    const to = Date.now();
    return observeApi.spans({ query: `@meta.span.kind:*${appQuery(app)}${sessionQuery(session)}`, from: to - RANGES[range], to, limit: STEPS }, signal);
  },
  opts: { pollMs: pollFor(range) },
});

/** A session (its spans' `session_id`) over the range: its steps, their cost and errors. */
interface Session {
  id: string;
  steps: number;
  cost: number;
  errors: number;
}

const SESSIONS = 20;

const sessionsRes = (app: string, range: Range): Res<Session[]> => ({
  key: `observe/llm/sessions/${range}/${app}`,
  fetch: async (signal) => {
    const to = Date.now();
    const from = to - RANGES[range];
    const q = `@meta.span.kind:* @session_id:*${appQuery(app)}`;
    const by = ["@session_id"];
    const [steps, cost, errors] = await Promise.all([
      observeApi.spanAggregate({ query: q, from, to, by }, signal),
      observeApi.spanAggregate({ query: q, from, to, by, measure: "sum", of: "@metrics.estimated_total_cost" }, signal),
      observeApi.spanAggregate({ query: `${q} status:error`, from, to, by }, signal),
    ]);
    const of = (a: SpanAggregate) => new Map(a.groups.map((g) => [g.group["@session_id"] ?? "", g.value ?? 0]));
    const [c, e] = [of(cost), of(errors)];
    return steps.groups
      .map((g) => {
        const id = g.group["@session_id"] ?? "";
        return { id, steps: g.value ?? 0, cost: c.get(id) ?? 0, errors: e.get(id) ?? 0 };
      })
      .sort((a, b) => b.cost - a.cost || b.steps - a.steps)
      .slice(0, SESSIONS);
  },
  opts: { pollMs: pollFor(range) },
});

const traceRes = (id: string): Res<Trace> => ({ key: `observe/trace/${id}`, fetch: (s) => observeApi.trace(id, s) });

/** An LLM trace's evaluations (records of `llmobs.eval`, by its LLM trace's id), from an hour before
 * it began: a judge may come long after. */
const evalsRes = (traceId: string, fromMs: number): Res<LogSearch> => ({
  key: `observe/llm/evals/${traceId}`,
  fetch: (signal) => observeApi.logs({ query: `source:llmobs.eval trace_id:${traceId}`, from: fromMs - 3_600_000, to: Date.now() + 60_000, limit: 200 }, signal),
  opts: { pollMs: 60_000 },
});

const took = (ns: number | string | undefined) => latency(ns === undefined ? null : Number(ns) / 1e9);
const usd = (nanodollars: number | string | undefined) => (nanodollars === undefined || nanodollars === "" ? "—" : formatUsd(Number(nanodollars) / 1e9));

export default function LlmObservability() {
  const [range, setRange] = useRange("1d");
  const [app, setApp] = useParam("app", "");
  const [session, setSession] = useParam("session", "");
  return (
    <div className="container-console page">
      <title>LLM · Observe · kiln</title>
      <PageIntro
        index={8}
        label="Observe"
        title="LLM"
        lead="An app's calls to language models, and its agents' and tools' steps: what they cost, their tokens, errors and latency, from the spans it sends in any convention (OpenTelemetry's GenAI, OpenInference, OpenLLMetry, Datadog's LLM Observability, Claude Code's)."
      />
      <ObserveNav />
      <div className="mb-5 flex flex-wrap items-center gap-3">
        <RangePicker value={range} onChange={setRange} />
        {app && (
          <button type="button" className="rounded-full border border-[#2a2d33] px-2.5 py-0.5 font-mono text-[11px] text-[#d6d8dd] hover:border-[#3a3e46]" onClick={() => setApp("")}>
            {app} ×
          </button>
        )}
        {session && (
          <button type="button" className="rounded-full border border-[#2a2d33] px-2.5 py-0.5 font-mono text-[11px] text-[#d6d8dd] hover:border-[#3a3e46]" onClick={() => setSession("")}>
            session {session} ×
          </button>
        )}
      </div>
      <Loadable key={`o/${range}/${app}`} label="the models" fallback={<RowsHold rows={6} rowHeight={36} />}>
        <OverviewView app={app} range={range} onApp={setApp} />
      </Loadable>
      <Panel title="Newest LLM traces" description="Each trace's first step: open one for its steps as a tree, each one's messages, tokens and cost.">
        <Loadable key={`t/${range}/${app}/${session}`} label="the traces" fallback={<RowsHold rows={8} rowHeight={36} />}>
          <TraceList app={app} session={session} range={range} />
        </Loadable>
      </Panel>
      {!session && (
        <Panel title="Sessions" description="Steps that say their session (a conversation's id), the costliest first: choose one for its traces.">
          <Loadable key={`s/${range}/${app}`} label="the sessions" fallback={<RowsHold rows={4} rowHeight={32} />}>
            <SessionList app={app} range={range} onSession={setSession} />
          </Loadable>
        </Panel>
      )}
    </div>
  );
}

function OverviewView({ app, range, onApp }: { app: string; range: Range; onApp: (a: string) => void }) {
  const o = useRes(overviewRes(app, range)).data;
  const sum = (f: (m: ModelRow) => number) => o.models.reduce((a, m) => a + f(m), 0);
  const [calls, errors] = [sum((m) => m.calls), sum((m) => m.errors)];
  if (o.apps.length === 0) {
    return (
      <p className="empty-line py-16 text-center">
        No LLM spans in this range. Send spans of an LLM app (OpenTelemetry's GenAI instrumentations, OpenInference, OpenLLMetry, Datadog's tracers with LLM Observability on) to kiln as any span.
      </p>
    );
  }
  const [tags, llm] = [tagsOf(app), tagsOf(app, "span_kind:llm")];
  return (
    <div className="space-y-6">
      <Facts
        items={[
          { label: "Calls", value: compact(calls) },
          { label: "Tokens", value: compact(sum((m) => m.input + m.output)) },
          { label: "Cost", value: formatUsd(sum((m) => m.cost) / 1e9) },
          { label: "Errors", value: calls > 0 ? `${compact((errors / calls) * 100)}%` : "—", tone: errors > 0 ? "fail" : undefined },
          { label: "Models", value: o.models.length.toLocaleString() },
        ]}
      />
      {!app && o.apps.length > 1 && (
        <ul className="flex flex-wrap gap-2" aria-label="Apps">
          {o.apps.map((a) => (
            <li key={a.app}>
              <button
                type="button"
                className="rounded-full border border-[#2a2d33] px-2.5 py-0.5 font-mono text-[11px] text-[#c8cad0] hover:border-[#3a3e46] hover:text-white"
                onClick={() => onApp(a.app)}
              >
                {a.app || "(no app)"} <span className="text-[#8c8f98]">{compact(a.spans)}</span>
              </button>
            </li>
          ))}
        </ul>
      )}
      <div className="grid grid-cols-1 gap-6 lg:grid-cols-2">
        <Panel title="Cost" description="By model, as the catalog prices its tokens, or as the app says it cost.">
          <QueryChart queries={[`sum:ml_obs.span.llm.total.cost.sum{${tags}} by {model_name}.as_count()`]} range={range} display="bars" unit="nanodollars" legend />
        </Panel>
        <Panel title="Tokens" description="Every input and output token of its calls, by model.">
          <QueryChart queries={[`sum:ml_obs.span.llm.total.tokens.sum{${tags}} by {model_name}.as_count()`]} range={range} display="bars" legend />
        </Panel>
        <Panel title="Calls and errors">
          <QueryChart queries={[`sum:ml_obs.span{${llm}}.as_count()`, `sum:ml_obs.span.error{${llm}}.as_count()`]} names={CALL_NAMES} range={range} display="bars" />
        </Panel>
        <Panel title="Latency" description="A call's, from its start to its last token.">
          <QueryChart queries={[`p50:ml_obs.span.duration{${llm}}`, `p95:ml_obs.span.duration{${llm}}`]} names={LATENCY_NAMES} range={range} unit="s" />
        </Panel>
      </div>
      <Panel title="Models">
        <div className="overflow-x-auto">
          <table className="w-full min-w-[640px] text-left text-[12.5px]">
            <thead className="text-[11px] text-[#8c8f98]">
              <tr>
                <th className="py-1.5 font-normal">Model</th>
                <th className="py-1.5 font-normal">Provider</th>
                <th className="py-1.5 text-right font-normal">Calls</th>
                <th className="py-1.5 text-right font-normal">Input tokens</th>
                <th className="py-1.5 text-right font-normal">Output tokens</th>
                <th className="py-1.5 text-right font-normal">Cost</th>
                <th className="py-1.5 text-right font-normal">Errors</th>
                <th className="py-1.5 text-right font-normal">Mean time</th>
              </tr>
            </thead>
            <tbody className="divide-y divide-[#1d1f23] border-y border-[#1d1f23] font-mono tabular-nums">
              {o.models.map((m) => (
                <tr key={`${m.model}/${m.provider}`}>
                  <td className="py-1.5 pr-3 text-[#e7e8ea]">{m.model || "(no model)"}</td>
                  <td className="py-1.5 pr-3 text-[#a7aab3]">{m.provider || "—"}</td>
                  <td className="py-1.5 text-right">{compact(m.calls)}</td>
                  <td className="py-1.5 text-right">{compact(m.input)}</td>
                  <td className="py-1.5 text-right">{compact(m.output)}</td>
                  <td className="py-1.5 text-right">{formatUsd(m.cost / 1e9)}</td>
                  <td className={cn("py-1.5 text-right", m.errors > 0 && "text-[#f27a7a]")}>{compact(m.errors)}</td>
                  <td className="py-1.5 text-right">{took(m.avgNs)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </Panel>
    </div>
  );
}

const CALL_NAMES = ["calls", "errors"] as const;
const LATENCY_NAMES = ["p50", "p95"] as const;

/** Each trace's first step (its LLM trace's: Datadog's tracer's says it), the newest traces first. */
function firstSteps(spans: Span[]): Span[] {
  const by = new Map<string, Span>();
  for (const s of spans) {
    const t = s.attributes["llmobs.trace_id"] ?? s.trace_id ?? "";
    const seen = by.get(t);
    if (!seen || s.ts_ns < seen.ts_ns) by.set(t, s);
  }
  return [...by.values()].sort((a, b) => b.ts_ns - a.ts_ns).slice(0, TRACES);
}

function SessionList({ app, range, onSession }: { app: string; range: Range; onSession: (s: string) => void }) {
  const sessions = useRes(sessionsRes(app, range)).data;
  if (sessions.length === 0) return <p className="empty-line py-6 text-center">No steps in this range say their session.</p>;
  return (
    <ol className="divide-y divide-[#1d1f23] border-y border-[#1d1f23]" aria-label="Sessions">
      {sessions.map((x) => (
        <li key={x.id}>
          <button type="button" onClick={() => onSession(x.id)} className="flex w-full items-baseline gap-3 px-2 py-1.5 text-left hover:bg-[#101114]">
            <span className="min-w-0 flex-1 truncate font-mono text-[12px] text-[#d6d8dd]">{x.id}</span>
            <span className="w-20 shrink-0 text-right font-mono text-[11px] text-[#a7aab3] tabular-nums">{compact(x.steps)} steps</span>
            <span className="w-24 shrink-0 text-right font-mono text-[11px] text-[#c8cad0] tabular-nums">{formatUsd(x.cost / 1e9)}</span>
            <span className={cn("hidden w-20 shrink-0 text-right font-mono text-[11px] tabular-nums sm:inline", x.errors > 0 ? "text-[#f27a7a]" : "text-[#8c8f98]")}>{compact(x.errors)} errors</span>
          </button>
        </li>
      ))}
    </ol>
  );
}

function TraceList({ app, session, range }: { app: string; session: string; range: Range }) {
  const found = useRes(tracesRes(app, session, range)).data;
  const firsts = React.useMemo(() => firstSteps(found.spans), [found]);
  if (firsts.length === 0) return <p className="empty-line py-10 text-center">No LLM traces in this range.</p>;
  return (
    <ol className="divide-y divide-[#1d1f23] border-y border-[#1d1f23]" aria-label="LLM traces">
      {firsts.map((s, i) => (
        <li key={s.span_id ?? i}>
          <Link to={s.trace_id ? `/observe/llm/traces/${s.trace_id}` : "/observe/llm"} className="flex w-full items-baseline gap-3 px-2 py-2 text-left hover:bg-[#101114]">
            <span className="inline-block size-2 shrink-0 translate-y-[-1px] rounded-full" style={{ background: STATUS_COLOR[s.status] }} title={s.status} aria-label={s.status} />
            <time className="shrink-0 font-mono text-[10.5px] text-[#8c8f98] tabular-nums" dateTime={new Date(s.ts_ns / 1e6).toISOString()}>
              {when(s.ts_ns)}
            </time>
            <Kind kind={s.attributes["meta.span.kind"]} />
            <span className="min-w-0 flex-1 truncate text-[12.5px] text-[#d6d8dd]">{nameOf(s)}</span>
            <span className="hidden w-28 shrink-0 truncate font-mono text-[10.5px] text-[#a7aab3] sm:inline">{s.attributes.ml_app ?? s.service}</span>
            <span className="w-16 shrink-0 text-right font-mono text-[11px] text-[#c8cad0] tabular-nums">{took(s.attributes.duration)}</span>
          </Link>
        </li>
      ))}
    </ol>
  );
}

/** A step's name: its LLM event's (Datadog's tracer's), else its span's. */
const nameOf = (s: Span | PlacedSpan) => s.attributes["llmobs.name"] ?? ("resource_name" in s ? s.resource_name : s.message);

const KIND_COLOR: Record<string, string> = { llm: "#b8a6d5", embedding: "#a6c4ed", retrieval: "#b8d9d2", tool: "#d8cbb0", agent: "#d6a2aa", workflow: "#a6b2c6", task: "#8d9097" };

function Kind({ kind }: { kind: string | undefined }) {
  return (
    <span className="w-[72px] shrink-0 font-mono text-[10.5px]" style={{ color: KIND_COLOR[kind ?? ""] ?? "#8d9097" }}>
      {kind ?? "span"}
    </span>
  );
}

export function LlmTracePage() {
  const { id = "" } = useParams();
  return (
    <div className="container-console page">
      <title>{`LLM trace ${id.slice(0, 8)} · Observe · kiln`}</title>
      <header className="page-intro">
        <div className="min-w-0">
          <Crumbs index={8} items={[{ label: "Observe", to: "/observe" }, { label: "LLM", to: "/observe/llm" }, { label: id.slice(0, 16) }]} />
        </div>
      </header>
      <Loadable key={id} label="the trace" fallback={<Hold height={480} label="Loading the trace" />}>
        <LlmTraceView id={id} />
      </Loadable>
    </div>
  );
}

/** A step of an LLM trace: its span, its depth among the trace's LLM steps. */
interface Step {
  span: PlacedSpan;
  depth: number;
}

/** A trace's LLM steps as a tree: each under its LLM parent (Datadog's tracer's says it, any other's
 * is its own parent where that is a step), in the order they began. */
function steps(t: Trace): Step[] {
  const llm = t.spans.filter((s) => s.attributes["meta.span.kind"]);
  const ids = new Set(llm.map((s) => s.span_id));
  const parentOf = (s: PlacedSpan) => {
    const p = s.attributes["llmobs.parent_id"] ?? (s.parent !== null ? t.spans[s.parent]?.span_id : undefined) ?? undefined;
    return p && ids.has(p) ? p : null;
  };
  const children = new Map<string | null, PlacedSpan[]>();
  for (const s of llm) {
    const p = parentOf(s);
    const list = children.get(p) ?? [];
    list.push(s);
    children.set(p, list);
  }
  const out: Step[] = [];
  const walk = (parent: string | null, depth: number) => {
    for (const s of (children.get(parent) ?? []).sort((a, b) => a.ts_ns - b.ts_ns)) {
      out.push({ span: s, depth });
      if (s.span_id && depth < 64) walk(s.span_id, depth + 1);
    }
  };
  walk(null, 0);
  return out;
}

function LlmTraceView({ id }: { id: string }) {
  const t = useRes(traceRes(id)).data;
  const [spanParam, setSpanParam] = useParam("span");
  const all = React.useMemo(() => steps(t), [t]);
  const picked = all.find((s) => s.span.span_id === spanParam) ?? all[0];
  const num = (s: Step, k: string) => Number(s.span.attributes[k] ?? 0) || 0;
  const tokens = all.reduce((a, s) => a + (s.span.attributes["meta.span.kind"] === "llm" ? num(s, "metrics.total_tokens") : 0), 0);
  const cost = all.reduce((a, s) => a + num(s, "metrics.estimated_total_cost"), 0);
  const root = all[0]?.span;
  if (!root) return <p className="empty-line py-16 text-center">This trace holds no LLM steps.</p>;
  return (
    <div className="space-y-6">
      <div>
        <h1 className="truncate text-[20px] text-[#f2f2f3]">
          <span className="font-mono text-[#a7aab3]">{root.attributes.ml_app ?? root.service}</span> {nameOf(root)}
        </h1>
        <Facts
          className="mt-3"
          items={[
            { label: "Duration", value: took(root.duration_ns) },
            { label: "Steps", value: all.length.toLocaleString() },
            { label: "Tokens", value: compact(tokens) },
            { label: "Cost", value: formatUsd(cost / 1e9) },
            { label: "Errors", value: all.filter((s) => s.span.status === "error").length.toLocaleString(), tone: all.some((s) => s.span.status === "error") ? "fail" : undefined },
            { label: "Started", value: when(root.ts_ns) },
          ]}
        />
      </div>
      <ol className="divide-y divide-[#1d1f23] border-y border-[#1d1f23]" aria-label="Steps">
        {all.map((s) => (
          <li key={s.span.span_id ?? s.span.ts_ns}>
            <button
              type="button"
              aria-current={s === picked}
              onClick={() => setSpanParam(s.span.span_id ?? "")}
              className={cn("flex w-full items-baseline gap-3 py-1.5 pr-2 text-left hover:bg-[#101114]", s === picked && "bg-[#121318]")}
              style={{ paddingLeft: 8 + Math.min(s.depth, 12) * 16 }}
            >
              <span className="inline-block size-2 shrink-0 translate-y-[-1px] rounded-full" style={{ background: STATUS_COLOR[s.span.status] }} aria-label={s.span.status} />
              <Kind kind={s.span.attributes["meta.span.kind"]} />
              <span className="min-w-0 flex-1 truncate text-[12.5px] text-[#d6d8dd]">{nameOf(s.span)}</span>
              <span className="hidden shrink-0 truncate font-mono text-[10.5px] text-[#a7aab3] md:inline">{s.span.attributes["meta.model_name"]}</span>
              <span className="hidden w-16 shrink-0 text-right font-mono text-[10.5px] text-[#a7aab3] tabular-nums sm:inline">
                {s.span.attributes["metrics.total_tokens"] ? compact(num(s, "metrics.total_tokens")) : ""}
              </span>
              <span className="hidden w-20 shrink-0 text-right font-mono text-[10.5px] text-[#a7aab3] tabular-nums sm:inline">
                {s.span.attributes["metrics.estimated_total_cost"] ? usd(s.span.attributes["metrics.estimated_total_cost"]) : ""}
              </span>
              <span className="w-16 shrink-0 text-right font-mono text-[11px] text-[#c8cad0] tabular-nums">{took(s.span.duration_ns)}</span>
            </button>
          </li>
        ))}
      </ol>
      {picked && <StepDetail s={picked.span} traceId={root.attributes["llmobs.trace_id"] ?? root.trace_id ?? ""} />}
    </div>
  );
}

/** A message as a step's input or output says it: its role, its text, the tools it calls. */
interface Message {
  role: string;
  text: string;
  calls: { name: string; args: string }[];
}

function parseJson(text: string | undefined): unknown {
  if (!text) return undefined;
  try {
    return JSON.parse(text);
  } catch {
    return undefined;
  }
}

const str = (v: unknown) => (typeof v === "string" ? v : v === undefined || v === null ? "" : JSON.stringify(v));

/** A message of Datadog's (`content`, `tool_calls`) or OpenTelemetry's (`parts`) form. */
function messageOf(m: unknown): Message | null {
  if (!m || typeof m !== "object") return null;
  const o = m as Record<string, unknown>;
  const calls: Message["calls"] = [];
  let text = "";
  if (Array.isArray(o.parts)) {
    for (const p of o.parts as Record<string, unknown>[]) {
      if (p?.type === "tool_call") calls.push({ name: str(p.name), args: str(p.arguments) });
      else if (p?.type === "tool_call_response") text += str(p.response);
      else text += str(p?.content ?? p?.text ?? "");
    }
  } else {
    text = str(o.content);
  }
  for (const c of Array.isArray(o.tool_calls) ? (o.tool_calls as Record<string, unknown>[]) : []) {
    const f = (c?.function ?? c) as Record<string, unknown>;
    calls.push({ name: str(f?.name), args: str(f?.arguments) });
  }
  return { role: str(o.role) || "message", text, calls };
}

/** The JSON arrays a step's events hold of one side's messages: OpenTelemetry's inference details
 * (1.37 on, `gen_ai.client.inference.operation.details`), or its first conventions' content events
 * (1.26, 1.27: `gen_ai.content.prompt`, `.completion`). */
function eventMessages(a: Record<string, string>, side: "input" | "output"): unknown[] {
  const events = parseJson(a.events);
  if (!Array.isArray(events)) return [];
  const attr = (name: string, key: string) => {
    const e = events.find((x) => (x as Record<string, unknown>)?.name === name) as { attributes?: Record<string, string> } | undefined;
    return parseJson(e?.attributes?.[key]);
  };
  return [
    attr("gen_ai.client.inference.operation.details", `gen_ai.${side}.messages`),
    side === "input" ? attr("gen_ai.content.prompt", "gen_ai.prompt") : attr("gen_ai.content.completion", "gen_ai.completion"),
  ];
}

/** A step's messages on one side, in whichever convention it says them: Datadog's, OpenTelemetry's
 * (its system instructions first; as attributes, or in its events), OpenInference's and
 * OpenLLMetry's flattened ones; else its value. */
function messagesOf(a: Record<string, string>, side: "input" | "output"): Message[] {
  for (const v of [parseJson(a[`meta.${side}.messages`]), parseJson(a[`gen_ai.${side}.messages`]), ...eventMessages(a, side)]) {
    if (Array.isArray(v)) {
      const out = v.map(messageOf).filter((m): m is Message => m !== null);
      const system = side === "input" ? parseJson(a["gen_ai.system_instructions"]) : undefined;
      if (Array.isArray(system)) out.unshift({ role: "system", text: system.map((p) => str((p as Record<string, unknown>)?.content)).join(""), calls: [] });
      return out;
    }
  }
  // Flattened: OpenInference's `llm.input_messages.0.message.role`, OpenLLMetry's `gen_ai.prompt.0.role`.
  const flat =
    side === "input"
      ? [/^llm\.input_messages\.(\d+)\.message\.(role|content)$/, /^gen_ai\.prompt\.(\d+)\.(role|content)$/]
      : [/^llm\.output_messages\.(\d+)\.message\.(role|content)$/, /^gen_ai\.completion\.(\d+)\.(role|content)$/];
  for (const re of flat) {
    const by = new Map<number, Message>();
    for (const [k, v] of Object.entries(a)) {
      const m = re.exec(k);
      if (!m) continue;
      const i = Number(m[1]);
      const msg = by.get(i) ?? { role: "message", text: "", calls: [] };
      if (m[2] === "role") msg.role = v;
      else msg.text = v;
      by.set(i, msg);
    }
    if (by.size > 0) return [...by.entries()].sort((x, y) => x[0] - y[0]).map(([, m]) => m);
  }
  const value = a[`meta.${side}.value`] ?? a[`${side}.value`] ?? a[`traceloop.entity.${side}`];
  return value ? [{ role: side, text: value, calls: [] }] : [];
}

function Messages({ list }: { list: Message[] }) {
  if (list.length === 0) return <p className="text-[12px] text-[#8c8f98]">None said (an app may keep its messages to itself).</p>;
  return (
    <ol className="space-y-3">
      {list.map((m, i) => (
        <li key={i}>
          <p className="font-mono text-[10.5px] tracking-wide text-[#8c8f98] uppercase">{m.role}</p>
          {m.text && <p className="mt-1 text-[12.5px] leading-relaxed break-words whitespace-pre-wrap text-[#e7e8ea]">{m.text}</p>}
          {m.calls.map((c, j) => (
            <p key={j} className="mt-1 font-mono text-[11.5px] break-words text-[#d8cbb0]">
              {c.name}({c.args})
            </p>
          ))}
        </li>
      ))}
    </ol>
  );
}

function StepEvaluations({ traceId, spanId, fromMs }: { traceId: string; spanId: string; fromMs: number }) {
  const mine = useRes(evalsRes(traceId, fromMs)).data.records.filter((r) => r.span_id === spanId);
  if (mine.length === 0) return null;
  return (
    <section aria-label="Evaluations" className="mt-5">
      <h3 className="mb-2 text-[12px] text-[#a7aab3]">Evaluations</h3>
      <ul className="space-y-1.5">
        {mine.map((r, i) => (
          <li key={i} className="text-[12.5px] text-[#e7e8ea]">
            <span className="font-mono">{r.attributes.label}</span> <span className="font-mono tabular-nums">{r.attributes.value}</span>
            {r.attributes.assessment && <span className={cn("ml-2 font-mono text-[11px]", r.attributes.assessment === "fail" ? "text-[#f27a7a]" : "text-[#a8c9bc]")}>{r.attributes.assessment}</span>}
            {r.attributes.reasoning && <span className="ml-2 text-[#a7aab3]">{r.attributes.reasoning}</span>}
          </li>
        ))}
      </ul>
    </section>
  );
}

function StepDetail({ s, traceId }: { s: PlacedSpan; traceId: string }) {
  const a = s.attributes;
  const n = (k: string) => (a[k] !== undefined ? compact(Number(a[k])) : "—");
  const error = a["error.message"] ?? a["meta.error.message"];
  return (
    <Panel title={nameOf(s)} description={[a["meta.span.kind"], a["meta.model_name"], a["meta.model_provider"]].filter(Boolean).join(" · ")}>
      <Facts
        items={[
          { label: "Duration", value: took(s.duration_ns) },
          { label: "Input tokens", value: n("metrics.input_tokens") },
          { label: "Output tokens", value: n("metrics.output_tokens") },
          { label: "Cache read", value: n("metrics.cache_read_input_tokens") },
          { label: "Cost", value: a["metrics.estimated_total_cost"] !== undefined ? usd(a["metrics.estimated_total_cost"]) : (a.cost_unavailable ?? "—") },
          { label: "Status", value: s.status, tone: s.status === "error" ? "fail" : undefined },
        ]}
      />
      {error && <p className="mt-4 font-mono text-[12px] break-words text-[#f27a7a]">{error}</p>}
      <div className="mt-5 grid grid-cols-1 gap-6 lg:grid-cols-2">
        <section aria-label="Input">
          <h3 className="mb-2 text-[12px] text-[#a7aab3]">Input</h3>
          <Messages list={messagesOf(a, "input")} />
        </section>
        <section aria-label="Output">
          <h3 className="mb-2 text-[12px] text-[#a7aab3]">Output</h3>
          <Messages list={messagesOf(a, "output")} />
        </section>
      </div>
      {traceId && s.span_id && (
        <Loadable label="its evaluations" fallback={null}>
          <StepEvaluations traceId={traceId} spanId={s.span_id} fromMs={s.ts_ns / 1e6} />
        </Loadable>
      )}
      <p className="mt-5 text-[12px]">
        <Link to={`/observe/traces/${s.trace_id}${s.span_id ? `?span=${s.span_id}` : ""}`} className="text-[#a6c4ed] hover:text-white">
          Open its span in the trace
        </Link>
        {a.matched_model_name && (
          <span className="ml-3 text-[#8c8f98]">
            Priced as {a.matched_model_provider}/{a.matched_model_name} ({a.cost_source === "user" ? "its own cost" : "the catalog's prices"})
          </span>
        )}
      </p>
    </Panel>
  );
}
