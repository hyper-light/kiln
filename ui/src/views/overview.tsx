import * as React from "react";
import { Link, useNavigate } from "@/lib/router";
import { ArrowUpRight, Server } from "lucide-react";
import type { Overview as OverviewData } from "@/api/types";
import { Chart } from "@/components/charts/chart";
import { SERIES } from "@/components/charts/types";
import { Hold, RowsHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { EmptyState, Facts, PageIntro, Panel, SectionHeading } from "@/components/ui/misc";
import { StatusDot } from "@/components/ui/status";
import { useAuth } from "@/lib/auth";
import { agentsRes, historyRes, lastSamples, overviewRes, recentRunsRes, useAgentsLive, useMetricsHistory, useOverview, useRecentRuns, warm } from "@/lib/data";
import { formatBytes, formatDuration, formatPct, formatUptime } from "@/lib/format";
import { AgentMiniCard, RelTime, RunActivityRow, childStatus, diskPct, memPct } from "./shared";

/** Samples a runner card's sparklines show. */
const SPARK = 180;

export default function Overview() {
  return (
    <div className="container-console page">
      <title>Overview · kiln</title>
      <Loadable label="the overview" fallback={<OverviewHold />}>
        <OverviewBody />
      </Loadable>
    </div>
  );
}

function OverviewBody() {
  warm(overviewRes, recentRunsRes(14), agentsRes, historyRes);
  const ov = useOverview();
  const d = ov.data;
  return (
    <>
      <Intro lead={`kiln v${d.system.version} · up ${formatUptime(d.system.uptime_s)} · generation ${d.system.generation}. Every run, job and agent below updates live.`} title={d.system.hostname} />
      <Kpis d={d} />
      <div className="mt-8 grid gap-4 xl:grid-cols-[minmax(0,1.7fr)_minmax(0,1fr)]">
        <Throughput d={d} />
        <RecentRuns />
      </div>
      <Fleet />
      <ServerSection d={d} />
    </>
  );
}

function Intro({ title, lead }: { title: React.ReactNode; lead: React.ReactNode }) {
  return (
    <PageIntro
      index={1}
      label="Overview"
      title={title}
      lead={lead}
      actions={
        <Link to="/runs" className="text-link">
          All runs <ArrowUpRight size={15} />
        </Link>
      }
    />
  );
}

function Kpis({ d }: { d: OverviewData | null }) {
  const stats = d?.stats;
  const finished = stats ? stats.runs_succeeded_24h + stats.runs_failed_24h : 0;
  const success = stats && finished > 0 ? (stats.runs_succeeded_24h / finished) * 100 : null;
  return (
    <section className="kpi-row" aria-label="Key metrics">
      <Kpi label="Runs · 24h" value={stats?.runs_24h} foot={stats ? `${stats.runs_succeeded_24h} passed · ${stats.runs_failed_24h} failed` : undefined} />
      <Kpi
        label="Success rate"
        value={!stats ? undefined : success === null ? "—" : success.toFixed(1)}
        unit={success === null ? undefined : "%"}
        foot={stats ? `${finished} finished runs` : undefined}
      />
      <Kpi label="Median job" value={stats ? formatDuration(stats.median_job_seconds) : undefined} foot="p50 · last 24h" />
      <Kpi label="Queue depth" value={d?.queue.queued} foot={d ? (d.queue.queued ? "jobs waiting for a slot" : "nothing waiting") : undefined} />
      <Kpi label="Running" value={d?.queue.running} foot={d && stats ? `${stats.jobs_by_status.running ?? d.queue.running} jobs in flight` : undefined} live={Boolean(d?.queue.running)} />
      <Kpi
        label="Slots busy"
        value={d ? `${d.agents.busy}` : undefined}
        unit={d ? `/ ${d.agents.slots}` : undefined}
        foot={d ? `${d.agents.connected}/${d.agents.total} runners connected` : undefined}
      />
    </section>
  );
}

function Throughput({ d }: { d: OverviewData | null }) {
  const hourly = d?.stats.hourly;
  const chartData = React.useMemo(() => {
    const h = hourly ?? [];
    return [h.map((b) => b.ts), h.map((b) => b.succeeded + b.failed), h.map((b) => b.failed), h.map((b) => b.avg_seconds)];
  }, [hourly]);
  return (
    <Panel
      eyebrow="Throughput · 24h"
      title="Runs per hour"
      description="Succeeded and failed runs, hourly; average run time on the right axis."
      actions={
        <Link to="/observe/live" className="icon-btn" aria-label="Open the live metrics">
          <ArrowUpRight size={16} />
        </Link>
      }
    >
      {d ? (
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
        <Hold height={256} />
      )}
    </Panel>
  );
}

function RecentRunsFrame({ children }: { children: React.ReactNode }) {
  return (
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
      {children}
    </Panel>
  );
}

function RecentRuns() {
  const runs = useRecentRuns(14).data;
  return (
    <RecentRunsFrame>
      {runs.length === 0 ? (
        <p className="empty-line py-10 text-center">No runs yet. Add a repository to get started.</p>
      ) : (
        <div className="-mx-1 max-h-[300px] divide-y divide-[#1d1f23] overflow-y-auto" aria-live="polite">
          {runs.map((r) => (
            <RunActivityRow key={r.id} run={r} />
          ))}
        </div>
      )}
    </RecentRunsFrame>
  );
}

function FleetHeading() {
  return (
    <SectionHeading
      index="02"
      label="Fleet"
      title="Runners"
      aside={
        <Link to="/runners" className="text-link">
          Manage fleet <ArrowUpRight size={15} />
        </Link>
      }
    />
  );
}

function Fleet() {
  const navigate = useNavigate();
  const agents = useAgentsLive().data;
  const history = useMetricsHistory().data;
  // Job pods are runners too, but they are listed under their jobs.
  const runners = agents.filter((a) => !a.ephemeral);
  return (
    <section className="mt-16">
      <FleetHeading />
      {runners.length === 0 ? (
        <EmptyState
          icon={<Server size={24} />}
          title="No runners yet."
          action={
            <Link to="/runners/add" className="text-link">
              Add a machine <ArrowUpRight size={15} />
            </Link>
          }
        >
          Jobs run on runners. Join a machine, or let kiln launch pods on Kubernetes.
        </EmptyState>
      ) : (
        <div className="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-3 2xl:grid-cols-4">
          {[...runners]
            .sort((a, b) => Number(b.connected) - Number(a.connected) || a.name.localeCompare(b.name))
            .slice(0, 8)
            .map((a) => (
              <AgentMiniCard key={a.name} agent={a} history={lastSamples(history, a.name, SPARK)} onOpen={() => navigate(`/runners?agent=${encodeURIComponent(a.name)}`)} />
            ))}
        </div>
      )}
    </section>
  );
}

function ServerHeadings({ children }: { children: [React.ReactNode, React.ReactNode] }) {
  return (
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
        {children[0]}
      </div>
      <div>
        <SectionHeading index="04" label="Host" title="Server host" />
        <Panel>
          <div className="pt-5">{children[1]}</div>
        </Panel>
      </div>
    </section>
  );
}

function ServerSection({ d }: { d: OverviewData }) {
  const srv = d.system.sample;
  return (
    <ServerHeadings>
      <div className="row-list">
        {d.system.children.map((c) => (
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
        <p className="empty-line py-6">No sample yet.</p>
      )}
    </ServerHeadings>
  );
}

/** The overview while it loads: its frame (titles, labels, panels) with the loader in each part. */
function OverviewHold() {
  const me = useAuth((s) => (s.kind === "authed" ? s.me : null));
  return (
    <>
      <Intro
        title={me?.hostname ?? <span className="invisible">kiln</span>}
        lead={<span className="invisible">kiln v{me?.version ?? "0.0.0"} · up 0d 0h · generation 0. Every run, job and agent below updates live.</span>}
      />
      <Kpis d={null} />
      <div className="mt-8 grid gap-4 xl:grid-cols-[minmax(0,1.7fr)_minmax(0,1fr)]">
        <Throughput d={null} />
        <RecentRunsFrame>
          <Hold height={300} />
        </RecentRunsFrame>
      </div>
      <section className="mt-16">
        <FleetHeading />
        <Hold height={196} />
      </section>
      <ServerHeadings>
        <RowsHold rows={6} rowHeight={45} />
        <Hold height={112} />
      </ServerHeadings>
    </>
  );
}

/** One figure. `live`: it has a dot for "now", lit while it is (kept in place either way, so the label doesn't move). */
function Kpi({ label, value, unit, foot, live }: { label: string; value: React.ReactNode | undefined; unit?: string; foot?: string; live?: boolean }) {
  return (
    <div className="kpi">
      <div className="label flex items-center gap-2">
        {live !== undefined && <i className={live ? "dot" : "dot invisible"} data-status="running" aria-hidden="true" />}
        {label}
      </div>
      <div className="kpi-value">
        {value === undefined ? "\u00a0" : value}
        {value !== undefined && unit && <small>{unit}</small>}
      </div>
      <div className="kpi-foot">{foot ?? "\u00a0"}</div>
    </div>
  );
}
