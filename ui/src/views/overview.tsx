import * as React from "react";
import { Link, useNavigate } from "react-router";
import { ArrowUpRight, Server } from "lucide-react";
import { api } from "@/api/client";
import { useServerEvents } from "@/api/events";
import type { Agent, HostSample, Run } from "@/api/types";
import { Chart } from "@/components/charts/chart";
import { SERIES } from "@/components/charts/types";
import { EmptyState, ErrorState, Facts, PageIntro, Panel, SectionHeading, Skeleton } from "@/components/ui/misc";
import { StatusDot } from "@/components/ui/status";
import { formatBytes, formatDuration, formatPct, formatUptime } from "@/lib/format";
import { useDocumentTitle, useResource } from "@/lib/hooks";
import { AgentMiniCard, RelTime, RunActivityRow, childStatus, diskPct, memPct, upsert } from "./shared";

const HISTORY_CAP = 180;

export default function Overview() {
  useDocumentTitle("Overview");
  const navigate = useNavigate();
  const ov = useResource((s) => api.overview(s), [], { pollMs: 30_000 });
  const runs = useResource((s) => api.runs({ limit: 14 }, s), []);
  const agents = useResource((s) => api.agents(s), [], { pollMs: 60_000 });
  const [history, setHistory] = React.useState<Record<string, HostSample[]>>({});

  React.useEffect(() => {
    const ctl = new AbortController();
    api
      .metricsHistory(undefined, ctl.signal)
      .then((h) => setHistory(Object.fromEntries(Object.entries(h).map(([k, v]) => [k, v.slice(-HISTORY_CAP)]))))
      .catch(() => undefined);
    return () => ctl.abort();
  }, []);

  useServerEvents((ev) => {
    switch (ev.type) {
      case "queue":
        ov.setData((d) => (d ? { ...d, queue: { queued: ev.queued, running: ev.running } } : d));
        break;
      case "run":
        runs.setData((list) => upsert<Run>(list, ev.run, (r) => r.id).sort((a, b) => b.created_at - a.created_at).slice(0, 14));
        break;
      case "agent":
        agents.setData((list) => upsert<Agent>(list, ev.agent, (a) => a.name, false));
        break;
      case "sample":
        setHistory((h) => ({ ...h, [ev.agent]: [...(h[ev.agent] ?? []), ev.sample].slice(-HISTORY_CAP) }));
        if (ev.agent === "server") ov.setData((d) => (d ? { ...d, system: { ...d.system, sample: ev.sample } } : d));
        else agents.setData((list) => list?.map((a) => (a.name === ev.agent ? { ...a, sample: ev.sample } : a)));
        break;
      case "job":
        break;
    }
  });

  const d = ov.data;
  const stats = d?.stats;
  const finished = stats ? stats.runs_succeeded_24h + stats.runs_failed_24h : 0;
  const success = stats && finished > 0 ? (stats.runs_succeeded_24h / finished) * 100 : null;

  const chartData = React.useMemo(() => {
    const h = stats?.hourly ?? [];
    return [h.map((b) => b.ts), h.map((b) => b.succeeded + b.failed), h.map((b) => b.failed), h.map((b) => b.avg_seconds)];
  }, [stats?.hourly]);

  const busySlots = d ? d.agents.busy : 0;
  const totalSlots = d ? d.agents.slots : 0;
  const srv = d?.system.sample;

  return (
    <div className="container-console page">
      <PageIntro
        index={1}
        label="Overview"
        title={d ? <>{d.system.hostname}</> : <Skeleton className="h-12 w-72" />}
        lead={
          d ? (
            <>
              kiln v{d.system.version} · up {formatUptime(d.system.uptime_s)} · generation {d.system.generation}. Every run, job and agent below updates live.
            </>
          ) : undefined
        }
        actions={
          <Link to="/runs" className="text-link">
            All runs <ArrowUpRight size={15} />
          </Link>
        }
      />

      {ov.error && !d && <ErrorState error={ov.error} onRetry={ov.reload} />}

      {/* KPI row */}
      <section className="kpi-row" aria-label="Key metrics" data-reveal>
        <Kpi label="Runs · 24h" value={stats?.runs_24h} foot={stats ? `${stats.runs_succeeded_24h} passed · ${stats.runs_failed_24h} failed` : undefined} />
        <Kpi
          label="Success rate"
          value={success === null ? (stats ? "—" : undefined) : success.toFixed(1)}
          unit={success === null ? undefined : "%"}
          foot={stats ? `${finished} finished runs` : undefined}
        />
        <Kpi label="Median job" value={stats ? formatDuration(stats.median_job_seconds) : undefined} foot="p50 · last 24h" />
        <Kpi label="Queue depth" value={d?.queue.queued} foot={d ? (d.queue.queued ? "jobs waiting for a slot" : "nothing waiting") : undefined} />
        <Kpi label="Running" value={d?.queue.running} foot={stats ? `${stats.jobs_by_status.running ?? d?.queue.running ?? 0} jobs in flight` : undefined} live={Boolean(d?.queue.running)} />
        <Kpi
          label="Slots busy"
          value={d ? `${busySlots}` : undefined}
          unit={d ? `/ ${totalSlots}` : undefined}
          foot={d ? `${d.agents.connected}/${d.agents.total} agents connected` : undefined}
        />
      </section>

      <div className="mt-8 grid gap-4 xl:grid-cols-[minmax(0,1.7fr)_minmax(0,1fr)]">
        <Panel
          eyebrow="Throughput · 24h"
          title="Runs per hour"
          description="Succeeded and failed runs, hourly; average run time on the right axis."
          actions={
            <Link to="/telemetry" className="icon-btn" aria-label="Open metrics">
              <ArrowUpRight size={16} />
            </Link>
          }
        >
          {stats ? (
            <Chart
              ariaLabel="Hourly succeeded and failed runs"
              data={chartData}
              timeFormat="hour"
              height={230}
              yFormat={(v) => String(Math.round(v))}
              y2Format={(v) => formatDuration(v)}
              series={[
                {
                  label: "Succeeded",
                  color: SERIES.ok,
                  bars: true,
                  value: (data, _v, _si, i) => {
                    const t = data[1]?.[i];
                    const f = data[2]?.[i];
                    return t == null ? "—" : String((t ?? 0) - (f ?? 0));
                  },
                },
                { label: "Failed", color: SERIES.fail, bars: true },
                { label: "Avg time", color: SERIES.violet, scale: "y2", width: 1.25, dash: [3, 3] },
              ]}
            />
          ) : (
            <Skeleton className="h-[256px] w-full" />
          )}
        </Panel>

        <Panel
          eyebrow="Activity"
          title="Recent runs"
          actions={
            <Link to="/runs" className="icon-btn" aria-label="All runs">
              <ArrowUpRight size={16} />
            </Link>
          }
          bodyClassName="pb-2"
        >
          {runs.error && !runs.data ? (
            <ErrorState error={runs.error} onRetry={runs.reload} compact />
          ) : !runs.data ? (
            <div className="space-y-3">
              {Array.from({ length: 7 }, (_, i) => (
                <Skeleton key={i} className="h-8 w-full" />
              ))}
            </div>
          ) : runs.data.length === 0 ? (
            <p className="py-10 text-center text-[13px] text-[#8f939e]">No runs yet. Add a repository to get started.</p>
          ) : (
            <div className="-mx-1 max-h-[300px] divide-y divide-[#1d1f23] overflow-y-auto" aria-live="polite">
              {runs.data.slice(0, 14).map((r) => (
                <RunActivityRow key={r.id} run={r} />
              ))}
            </div>
          )}
        </Panel>
      </div>

      {/* Fleet */}
      <section className="mt-16">
        <SectionHeading
          index="02"
          label="Fleet"
          title="Agents"
          aside={
            <Link to="/agents" className="text-link">
              Manage fleet <ArrowUpRight size={15} />
            </Link>
          }
        />
        {agents.error && !agents.data ? (
          <ErrorState error={agents.error} onRetry={agents.reload} />
        ) : !agents.data ? (
          <div className="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-4">
            {Array.from({ length: 4 }, (_, i) => (
              <Skeleton key={i} className="h-[190px]" />
            ))}
          </div>
        ) : agents.data.length === 0 ? (
          <EmptyState icon={<Server size={24} />} title="No agents yet." action={<Link to="/agents?join=1" className="text-link">Add a bare-metal agent <ArrowUpRight size={15} /></Link>}>
            Jobs run on agents. Join a bare-metal machine, or let kiln launch pods on Kubernetes.
          </EmptyState>
        ) : (
          <div className="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-3 2xl:grid-cols-4">
            {[...agents.data]
              .sort((a, b) => Number(b.connected) - Number(a.connected) || b.busy - a.busy || a.name.localeCompare(b.name))
              .slice(0, 8)
              .map((a) => (
                <AgentMiniCard key={a.name} agent={a} history={history[a.name] ?? []} onOpen={() => navigate(`/agents?agent=${encodeURIComponent(a.name)}`)} />
              ))}
          </div>
        )}
      </section>

      {/* Server */}
      <section className="mt-16 grid gap-4 lg:grid-cols-[minmax(0,1.2fr)_minmax(0,1fr)]">
        <div>
          <SectionHeading
            index="03"
            label="Supervisor"
            title="Server subsystems"
            aside={
              <Link to="/system" className="text-link">
                System <ArrowUpRight size={15} />
              </Link>
            }
          />
          <div className="row-list">
            {!d
              ? Array.from({ length: 5 }, (_, i) => (
                  <div key={i} className="py-3">
                    <Skeleton className="h-4 w-full" />
                  </div>
                ))
              : d.system.children.map((c) => (
                  <div key={c.name} className="grid grid-cols-[auto_minmax(0,1fr)_auto_auto] items-center gap-4 px-1 py-3">
                    <StatusDot status={childStatus(c)} title={c.state} />
                    <div className="min-w-0">
                      <div className="truncate text-[13px] text-[#e4e5e7]">{c.name}</div>
                      {c.last_error && <div className="truncate font-mono text-[10px] text-[#cbb69a]">{c.last_error}</div>}
                    </div>
                    <span className="font-mono text-[10px] text-[#8c8f98]">{c.restarts ? `${c.restarts}× restarted` : c.state}</span>
                    <span className="w-16 text-right font-mono text-[10px] text-[#7b7e88]">
                      <RelTime ts={c.since_unix} />
                    </span>
                  </div>
                ))}
          </div>
        </div>
        <div>
          <SectionHeading index="04" label="Host" title="Server host" />
          <Panel>
            <div className="pt-5">
              {srv ? (
                <Facts
                  items={[
                    { label: "CPU", value: `${formatPct(srv.cpu_pct, 1)} of ${srv.cpus}` },
                    { label: "Load (1m)", value: srv.load1.toFixed(2), tone: srv.load1 > srv.cpus ? "error" : undefined },
                    { label: "Memory", value: `${formatBytes(srv.mem_used)} / ${formatBytes(srv.mem_total)}`, tone: memPct(srv) > 90 ? "fail" : undefined },
                    { label: "Disk", value: `${formatPct(diskPct(srv))} of ${formatBytes(srv.disk_total, 0)}`, tone: diskPct(srv) > 90 ? "fail" : undefined },
                    { label: "Processes", value: srv.procs },
                    { label: "Sampled", value: <RelTime ts={Math.floor(srv.ts_ms / 1000)} /> },
                  ]}
                />
              ) : (
                <Skeleton className="h-28 w-full" />
              )}
            </div>
          </Panel>
        </div>
      </section>
    </div>
  );
}

function Kpi({ label, value, unit, foot, live }: { label: string; value: React.ReactNode | undefined; unit?: string; foot?: string; live?: boolean }) {
  return (
    <div className="kpi">
      <div className="label flex items-center gap-2">
        {live && <i className="dot" data-status="running" aria-hidden="true" />}
        {label}
      </div>
      <div className="kpi-value">
        {value === undefined ? <Skeleton className="h-8 w-20" /> : value}
        {value !== undefined && unit && <small>{unit}</small>}
      </div>
      <div className="kpi-foot">{foot ?? " "}</div>
    </div>
  );
}
