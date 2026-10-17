import * as React from "react";
import { Link } from "react-router";
import { ArrowUpRight, Settings } from "lucide-react";
import { api } from "@/api/client";
import { queueHistory, useServerEvents } from "@/api/events";
import type { HostSample } from "@/api/types";
import { Chart } from "@/components/charts/chart";
import { SERIES } from "@/components/charts/types";
import { Button } from "@/components/ui/button";
import { ErrorState, PageIntro, Panel, SectionHeading, SelectField, Skeleton } from "@/components/ui/misc";
import { Segmented } from "@/components/ui/tabs";
import { formatDuration } from "@/lib/format";
import { readLocal, useDocumentTitle, useResource } from "@/lib/hooks";
import { HostCharts } from "./host-charts";

const RANGES = { "5m": 300, "15m": 900, "30m": 1800, "1h": 3600 } as const;
type Range = keyof typeof RANGES;
const FLEET_COLORS = [SERIES.violet, SERIES.blue, SERIES.teal, SERIES.rose, SERIES.sand, SERIES.ok, SERIES.pending, SERIES.warn];

export default function Metrics() {
  useDocumentTitle("Metrics");
  const [range, setRange] = React.useState<Range>("15m");
  const [source, setSource] = React.useState("server");
  const hist = useResource((s) => api.metricsHistory(undefined, s), [], { pollMs: 120_000 });
  const ov = useResource((s) => api.overview(s), [], { pollMs: 60_000 });
  const [, bump] = React.useReducer((x: number) => x + 1, 0);
  const grafana = readLocal<string>("kiln.grafanaUrl", "");

  useServerEvents((ev) => {
    if (ev.type === "sample") {
      hist.setData((h) => (h ? { ...h, [ev.agent]: [...(h[ev.agent] ?? []), ev.sample].slice(-800) } : h));
    }
    if (ev.type === "queue") bump();
  });

  const cutoff = Date.now() - RANGES[range] * 1000;
  const sources = Object.keys(hist.data ?? {}).sort((a, b) => (a === "server" ? -1 : b === "server" ? 1 : a.localeCompare(b)));
  const samples: HostSample[] = React.useMemo(() => (hist.data?.[source] ?? []).filter((s) => s.ts_ms >= cutoff), [hist.data, source, cutoff]);

  // Fleet CPU comparison: align every agent to the server's timestamps (nearest prior sample).
  const fleet = React.useMemo(() => {
    const data = hist.data ?? {};
    const names = Object.keys(data)
      .filter((n) => n !== "server")
      .sort()
      .slice(0, 8);
    const base = (data.server ?? data[names[0] ?? ""] ?? []).filter((s) => s.ts_ms >= cutoff);
    const xs = base.map((s) => s.ts_ms / 1000);
    const ys = names.map((n) => {
      const arr = (data[n] ?? []).filter((s) => s.ts_ms >= cutoff - 10_000);
      let j = 0;
      return base.map((b) => {
        while (j + 1 < arr.length && arr[j + 1]!.ts_ms <= b.ts_ms) j++;
        const s = arr[j];
        return s && Math.abs(s.ts_ms - b.ts_ms) < 15_000 ? s.cpu_pct : null;
      });
    });
    return { names, data: [xs, ...ys] };
  }, [hist.data, cutoff]);

  const queue = queueHistory.filter((q) => q.ts * 1000 >= cutoff);
  const queueData = [queue.map((q) => q.ts), queue.map((q) => q.queued), queue.map((q) => q.running)];
  const hourly = ov.data?.stats.hourly ?? [];

  return (
    <div className="container-console page">
      <PageIntro
        index={8}
        label="Metrics"
        title="Metrics"
        lead="Host samples read straight from /proc and cgroups — the last hour at 5 s resolution, streamed live. Long-term history belongs in Prometheus + Grafana."
        actions={
          grafana ? (
            <Button asChild variant="secondary">
              <a href={grafana} target="_blank" rel="noopener noreferrer">
                Open Grafana <ArrowUpRight />
              </a>
            </Button>
          ) : (
            <Button asChild variant="ghost">
              <Link to="/settings">
                <Settings /> Link Grafana
              </Link>
            </Button>
          )
        }
      />

      <div className="sticky top-[var(--header-h)] z-10 -mx-2 mb-6 flex flex-wrap items-center gap-3 bg-[#08090a]/90 px-2 py-2 backdrop-blur">
        <Segmented label="Time range" value={range} onChange={setRange} options={(Object.keys(RANGES) as Range[]).map((r) => ({ value: r, label: r }))} />
        <SelectField
          label="Source"
          hideLabel
          className="w-full sm:ml-auto sm:w-64"
          value={source}
          onChange={setSource}
          options={(sources.length ? sources : ["server"]).map((s) => ({ value: s, label: s === "server" ? "server (kiln host)" : s }))}
        />
      </div>

      {hist.error && !hist.data ? (
        <ErrorState error={hist.error} onRetry={hist.reload} />
      ) : !hist.data ? (
        <div className="grid gap-4 lg:grid-cols-2">
          {Array.from({ length: 4 }, (_, i) => (
            <Skeleton key={i} className="h-[230px]" />
          ))}
        </div>
      ) : samples.length < 2 ? (
        <p className="py-10 text-center text-[13px] text-[#8f939e]">No samples for {source} in this range yet.</p>
      ) : (
        <HostCharts samples={samples} height={170} columns={3} />
      )}

      <section className="mt-14">
        <SectionHeading index="02" label="Scheduler" title="Queue & throughput" />
        <div className="grid gap-4 lg:grid-cols-2">
          <Panel eyebrow="Queue depth" title={ov.data ? `${ov.data.queue.queued} queued · ${ov.data.queue.running} running` : "—"} description="Recorded from the live event stream since this console opened.">
            {queue.length < 2 ? (
              <p className="py-12 text-center font-mono text-[11px] text-[#7f8a9a]">Waiting for queue events…</p>
            ) : (
              <Chart
                ariaLabel="Queue depth"
                data={queueData}
                height={170}
                yFormat={(v) => String(Math.round(v))}
                series={[
                  { label: "Queued", color: SERIES.pending, fill: true },
                  { label: "Running", color: SERIES.violet },
                ]}
              />
            )}
          </Panel>
          <Panel eyebrow="Throughput · 24h" title={ov.data ? `${ov.data.stats.runs_24h} runs · p50 job ${formatDuration(ov.data.stats.median_job_seconds)}` : "—"}>
            {!ov.data ? (
              <Skeleton className="h-[196px] w-full" />
            ) : (
              <Chart
                ariaLabel="Hourly throughput"
                data={[hourly.map((h) => h.ts), hourly.map((h) => h.succeeded + h.failed), hourly.map((h) => h.failed), hourly.map((h) => h.avg_seconds)]}
                height={170}
                timeFormat="hour"
                yFormat={(v) => String(Math.round(v))}
                y2Format={(v) => formatDuration(v)}
                series={[
                  { label: "Runs", color: SERIES.ok, bars: true },
                  { label: "Failed", color: SERIES.fail, bars: true },
                  { label: "Avg time", color: SERIES.violet, scale: "y2", dash: [3, 3] },
                ]}
              />
            )}
          </Panel>
        </div>
      </section>

      {fleet.names.length > 0 && (
        <section className="mt-14">
          <SectionHeading index="03" label="Fleet" title="CPU across agents" />
          <Panel>
            <div className="pt-4">
              <Chart
                ariaLabel="CPU by agent"
                data={fleet.data}
                height={220}
                yRange={[0, 100]}
                yFormat={(v) => `${Math.round(v)}%`}
                series={fleet.names.map((n, i) => ({ label: n, color: FLEET_COLORS[i % FLEET_COLORS.length]!, width: 1.1 }))}
              />
            </div>
          </Panel>
        </section>
      )}

      <p className="mt-10 font-mono text-[10px] text-[#7b7e88]">
        <span className="tiny-cross mr-2 align-middle">+</span>Prometheus exposition at{" "}
        <a className="text-[#d1c6e6] underline decoration-[#635b73] underline-offset-4 hover:text-[#f1e7ff]" href="/metrics" target="_blank" rel="noreferrer">
          /metrics
        </a>
      </p>
    </div>
  );
}
