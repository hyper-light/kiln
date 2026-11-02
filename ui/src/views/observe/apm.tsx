/**
 * APM (docs/observe.md §8.7): the services that send traces, each one's requests, errors and
 * latency over the range (from the trace metrics kiln counts of their spans), and a service's own
 * page: how its requests went over time, by resource, and the calls it makes. Traces and the
 * service map are a tab away.
 */
import { observeApi, type Red, type ServiceDetail, type Services } from "@/api/observe";
import { Button } from "@/components/ui/button";
import { Hold, TableHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { Crumbs, Facts, PageIntro, Panel } from "@/components/ui/misc";
import { Segmented, tabsTriggerClass } from "@/components/ui/tabs";
import { useRes, type Res } from "@/lib/data";
import { Link, NavLink, useParams } from "@/lib/router";
import { ObserveNav, QueryChart, RANGES, RangePicker, compact, formatSeconds, pollFor, useParam, useRange, type Range } from "./parts";

const TABS = [
  { to: "/observe/apm", label: "Services", end: true },
  { to: "/observe/traces", label: "Traces" },
  { to: "/observe/apm/map", label: "Map" },
];

/** APM's pages, the current one marked. */
export function ApmTabs() {
  return (
    <nav aria-label="APM" className="mb-6 inline-flex max-w-full gap-1 overflow-x-auto rounded-[22px] border border-white/10 bg-white/[0.02] p-1">
      {TABS.map((t) => (
        <NavLink key={t.to} to={t.to} end={t.end} className={`${tabsTriggerClass} aria-[current=page]:border-white/10 aria-[current=page]:bg-white/[0.07] aria-[current=page]:text-[#f2f2f3]`}>
          {t.label}
        </NavLink>
      ))}
    </nav>
  );
}

const servicesRes = (range: Range, env: string): Res<Services> => ({
  key: `observe/apm/services/${range}/${env}`,
  fetch: (s) => {
    const to = Date.now();
    return observeApi.services(to - RANGES[range], to, env, s);
  },
  opts: { pollMs: pollFor(range) },
});

const serviceRes = (name: string, range: Range, env: string): Res<ServiceDetail> => ({
  key: `observe/apm/service/${name}/${range}/${env}`,
  fetch: (s) => {
    const to = Date.now();
    return observeApi.service(name, to - RANGES[range], to, env, s);
  },
  opts: { pollMs: pollFor(range) },
});

/** A latency (s), or a dash. */
export const latency = (s: number | null | undefined) => (s === null || s === undefined ? "—" : formatSeconds(s));

/** Requests a second over the range. */
function perSecond(hits: number, range: Range) {
  const r = hits / (RANGES[range] / 1000);
  return r >= 10 ? `${compact(r)}/s` : `${r.toFixed(r >= 1 ? 1 : 2)}/s`;
}

/** An error rate as read: a share of requests, red past nothing. */
function ErrorRate({ r }: { r: Red }) {
  const rate = r.error_rate ?? 0;
  const text = r.hits === 0 ? "—" : rate === 0 ? "0%" : rate < 0.001 ? "<0.1%" : `${compact(rate * 100)}%`;
  return <span className={rate > 0 ? "text-[#f27a7a]" : "text-[#8c8f98]"}>{text}</span>;
}

/** The environment the pages are narrowed to (`?env=`), and the ones there are. */
function EnvPicker({ value, envs, onChange }: { value: string; envs: string[]; onChange: (e: string) => void }) {
  if (envs.length === 0) return null;
  return (
    <Segmented label="Environment" value={envs.includes(value) ? value : "*"} onChange={onChange} options={[{ value: "*", label: "All" }, ...envs.slice(0, 6).map((e) => ({ value: e, label: e }))]} />
  );
}

export default function ApmServices() {
  const [range, setRange] = useRange();
  const [env, setEnv] = useParam("env", "*");
  return (
    <div className="container-console page">
      <title>APM · Observe · kiln</title>
      <PageIntro
        index={8}
        label="Observe"
        title="APM"
        lead="Every service that sends traces: its requests, their errors and latency, from the trace metrics kiln counts of its spans. Open one for its resources and the calls it makes."
      />
      <ObserveNav />
      <ApmTabs />
      <div className="mb-5 flex flex-wrap items-center gap-3">
        <RangePicker value={range} onChange={setRange} />
      </div>
      <Loadable key={`${range}/${env}`} label="the services" fallback={<TableHold head={<ServicesHead />} rows={6} />}>
        <ServicesTable range={range} env={env} onEnv={setEnv} />
      </Loadable>
    </div>
  );
}

function ServicesHead() {
  return (
    <tr>
      <th>Service</th>
      <th className="text-right">Requests</th>
      <th className="text-right">Errors</th>
      <th className="hidden text-right sm:table-cell">p50</th>
      <th className="text-right">p95</th>
      <th className="hidden text-right md:table-cell">p99</th>
    </tr>
  );
}

function ServicesTable({ range, env, onEnv }: { range: Range; env: string; onEnv: (e: string) => void }) {
  const s = useRes(servicesRes(range, env)).data;
  return (
    <>
      {s.envs.length > 0 && (
        <div className="mb-4">
          <EnvPicker value={env} envs={s.envs} onChange={onEnv} />
        </div>
      )}
      {s.services.length === 0 ? (
        <p className="empty-line py-12 text-center">No traces in this range. A service sends them over OTLP to the server or to a runner's receiver (`/v1/traces`).</p>
      ) : (
        <div className="table-wrap">
          <table className="data-table">
            <thead>
              <ServicesHead />
            </thead>
            <tbody>
              {s.services.map((r) => (
                <tr key={r.service}>
                  <td className="truncate-cell">
                    <Link
                      to={`/observe/apm/services/${encodeURIComponent(r.service)}${env !== "*" ? `?env=${encodeURIComponent(env)}` : ""}`}
                      className="block truncate font-mono text-[12.5px] text-[#e4e5e7] hover:text-white"
                      title={r.service}
                    >
                      {r.service || "(no service)"}
                    </Link>
                  </td>
                  <td className="text-right font-mono text-[12px] whitespace-nowrap">
                    {compact(r.hits)}
                    <span className="ml-2 hidden text-[#8c8f98] sm:inline">{perSecond(r.hits, range)}</span>
                  </td>
                  <td className="text-right font-mono text-[12px] whitespace-nowrap">
                    <ErrorRate r={r} />
                  </td>
                  <td className="hidden text-right font-mono text-[12px] whitespace-nowrap sm:table-cell">{latency(r.p50_s)}</td>
                  <td className="text-right font-mono text-[12px] whitespace-nowrap">{latency(r.p95_s)}</td>
                  <td className="hidden text-right font-mono text-[12px] whitespace-nowrap md:table-cell">{latency(r.p99_s)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </>
  );
}

/** A tag filter for a query: the service's, and the environment's when narrowed. */
function filterOf(service: string, env: string) {
  const q = (v: string) => (/^[\w.\-/:]+$/.test(v) ? v : `"${v.replace(/["\\]/g, "\\$&")}"`);
  return [`service:${q(service)}`, env !== "*" ? `env:${q(env)}` : ""].filter(Boolean).join(",");
}

export function ApmService() {
  const { name = "" } = useParams();
  const [range, setRange] = useRange();
  const [env] = useParam("env", "*");
  return (
    <div className="container-console page">
      <title>{`${name} · APM · Observe · kiln`}</title>
      <header className="page-intro">
        <div className="min-w-0">
          <Crumbs index={8} items={[{ label: "Observe", to: "/observe" }, { label: "APM", to: "/observe/apm" }, { label: name }]} />
          <h1 className="truncate font-mono">{name}</h1>
        </div>
        <div className="flex flex-wrap gap-2">
          <Button variant="secondary" asChild>
            <Link to={`/observe/traces?q=${encodeURIComponent(`service:${name}`)}&range=${range}`}>Its traces</Link>
          </Button>
        </div>
      </header>
      <div className="mb-5">
        <RangePicker value={range} onChange={setRange} />
      </div>
      <Loadable key={`${name}/${range}/${env}`} label="the service" fallback={<Hold height={520} label="Loading the service" />}>
        <ServiceView name={name} range={range} env={env} />
      </Loadable>
    </div>
  );
}

function ServiceView({ name, range, env }: { name: string; range: Range; env: string }) {
  const d = useRes(serviceRes(name, range, env)).data;
  const f = filterOf(name, env);
  const s = d.summary;
  return (
    <div className="space-y-6">
      {s && (
        <Facts
          items={[
            { label: "Requests", value: `${compact(s.hits)} · ${perSecond(s.hits, range)}` },
            { label: "Errors", value: <ErrorRate r={s} />, tone: (s.error_rate ?? 0) > 0 ? "fail" : undefined },
            { label: "p50", value: latency(s.p50_s) },
            { label: "p95", value: latency(s.p95_s) },
            { label: "p99", value: latency(s.p99_s) },
          ]}
        />
      )}
      <div className="grid grid-cols-1 gap-6 lg:grid-cols-2">
        <Panel title="Requests and errors" description="Each ten seconds' requests the service served, and those that failed.">
          <Loadable label="the requests" fallback={<Hold height={180} />}>
            <QueryChart queries={[`sum:trace.hits{${f}}.as_count()`, `sum:trace.errors{${f}}.as_count()`]} range={range} display="bars" />
          </Loadable>
        </Panel>
        <Panel title="Latency" description="Its requests' 50th, 95th and 99th percentiles.">
          <Loadable label="the latency" fallback={<Hold height={180} />}>
            <QueryChart queries={[`p50:trace.duration{${f}}`, `p95:trace.duration{${f}}`, `p99:trace.duration{${f}}`]} range={range} unit="s" />
          </Loadable>
        </Panel>
      </div>
      <Panel title="Resources" description="What it serves, by name (an endpoint, a queue's topic): each one's requests over the range.">
        <RedTable
          rows={d.resources.map((r) => ({ ...r, key: r.resource_name }))}
          label="Resource"
          empty="No requests in this range."
          link={(k) => `/observe/traces?q=${encodeURIComponent(`service:${name} resource_name:"${k.replace(/["\\]/g, "\\$&")}"`)}&range=${range}`}
          range={range}
        />
      </Panel>
      <Panel title="Calls" description="What it calls: other services, databases, queues and hosts, as its clients' spans name them.">
        <RedTable rows={d.calls.map((r) => ({ ...r, key: r["peer.service"] }))} label="Calls to" empty="It made no calls its spans name in this range." range={range} />
      </Panel>
    </div>
  );
}

function RedTable({ rows, label, empty, link, range }: { rows: (Red & { key: string })[]; label: string; empty: string; link?: (key: string) => string; range: Range }) {
  if (rows.length === 0) return <p className="empty-line py-8 text-center">{empty}</p>;
  return (
    <div className="table-wrap">
      <table className="data-table">
        <thead>
          <tr>
            <th>{label}</th>
            <th className="text-right">Count</th>
            <th className="text-right">Errors</th>
            <th className="hidden text-right sm:table-cell">Avg</th>
            <th className="text-right">p95</th>
          </tr>
        </thead>
        <tbody>
          {rows.map((r) => (
            <tr key={r.key}>
              <td className="truncate-cell">
                {link ? (
                  <Link to={link(r.key)} className="block truncate font-mono text-[12px] text-[#e4e5e7] hover:text-white" title={r.key}>
                    {r.key}
                  </Link>
                ) : (
                  <span className="block truncate font-mono text-[12px] text-[#e4e5e7]" title={r.key}>
                    {r.key === "N/A" ? "(unnamed)" : r.key}
                  </span>
                )}
              </td>
              <td className="text-right font-mono text-[12px] whitespace-nowrap">
                {compact(r.hits)}
                <span className="ml-2 hidden text-[#8c8f98] sm:inline">{perSecond(r.hits, range)}</span>
              </td>
              <td className="text-right font-mono text-[12px] whitespace-nowrap">
                <ErrorRate r={r} />
              </td>
              <td className="hidden text-right font-mono text-[12px] whitespace-nowrap sm:table-cell">{latency(r.avg_s)}</td>
              <td className="text-right font-mono text-[12px] whitespace-nowrap">{latency(r.p95_s)}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
