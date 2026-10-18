import * as React from "react";
import { Link } from "@/lib/router";
import { ArrowUpRight, Settings } from "lucide-react";
import { queueHistory } from "@/api/events";
import type { HostSample } from "@/api/types";
import { Chart } from "@/components/charts/chart";
import { SERIES } from "@/components/charts/types";
import { Button } from "@/components/ui/button";
import { Hold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { PageIntro, Panel, SectionHeading, SelectField } from "@/components/ui/misc";
import { Segmented } from "@/components/ui/tabs";
import { historyRes, overviewRes, useMetricsHistory, useOverview, warm } from "@/lib/data";
import { formatDuration } from "@/lib/format";
import { usePref } from "@/lib/prefs";
import { HostCharts } from "./host-charts";

const RANGES = { "5m": 300, "15m": 900, "30m": 1800, "1h": 3600 } as const;
type Range = keyof typeof RANGES;
const FLEET_COLORS = [SERIES.violet, SERIES.blue, SERIES.teal, SERIES.rose, SERIES.sand, SERIES.ok, SERIES.pending, SERIES.warn];
const HOST_PANELS = ["CPU", "Memory", "Load (1m)", "Network", "Disk"];

export default function Metrics() {
  const [grafana] = usePref("kiln.grafanaUrl", "");
  const [range, setRange] = React.useState<Range>("15m");
  const [source, setSource] = React.useState("server");
  return (
    <div className="container-console page">
      <title>Metrics · kiln</title>
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
      <Loadable label="the metrics" fallback={<MetricsHold range={range} source={source} />}>
        <MetricsBody range={range} onRange={setRange} source={source} onSource={setSource} />
      </Loadable>
      <p className="mt-10 font-mono text-[10px] text-[#7b7e88]">
        <span className="tiny-cross mr-2 align-middle">+</span>Prometheus exposition at{" "}
        <a className="text-[#d1c6e6] underline decoration-[#635b73] underline-offset-4 hover:text-[#f1e7ff]" href="/metrics" target="_blank" rel="noreferrer">
          /metrics
        </a>
      </p>
    </div>
  );
}

function Toolbar({ range, onRange, source, onSource, sources }: { range: Range; onRange: (r: Range) => void; source: string; onSource: (s: string) => void; sources: string[] }) {
  return (
    <div className="mb-6 flex flex-wrap items-center gap-3">
      <Segmented label="Time range" value={range} onChange={onRange} options={(Object.keys(RANGES) as Range[]).map((r) => ({ value: r, label: r }))} />
      <SelectField
        label="Source"
        hideLabel
        className="w-full sm:ml-auto sm:w-64"
        value={source}
        onChange={onSource}
        options={(sources.length ? sources : ["server"]).map((s) => ({ value: s, label: s === "server" ? "server (kiln host)" : s }))}
      />
    </div>
  );
}

function MetricsBody({ range, onRange, source, onSource }: { range: Range; onRange: (r: Range) => void; source: string; onSource: (s: string) => void }) {
  warm(historyRes, overviewRes);
  const hist = useMetricsHistory().data;
  // Queue events change the overview (and so draw this again): the queue's own history follows.
  const ov = useOverview().data;

  const cutoff = Date.now() - RANGES[range] * 1000;
  const sources = Object.keys(hist).sort((a, b) => (a === "server" ? -1 : b === "server" ? 1 : a.localeCompare(b)));
  const samples: HostSample[] = React.useMemo(() => (hist[source] ?? []).filter((s) => s.ts_ms >= cutoff), [hist, source, cutoff]);

  // Fleet CPU comparison: align every agent to the server's timestamps (nearest prior sample).
  const fleet = React.useMemo(() => {
    const names = Object.keys(hist)
      .filter((n) => n !== "server")
      .sort()
      .slice(0, 8);
    const base = (hist.server ?? hist[names[0] ?? ""] ?? []).filter((s) => s.ts_ms >= cutoff);
    const xs = base.map((s) => s.ts_ms / 1000);
    const ys = names.map((n) => {
      const arr = (hist[n] ?? []).filter((s) => s.ts_ms >= cutoff - 10_000);
      let j = 0;
      return base.map((b) => {
        while (j + 1 < arr.length && arr[j + 1]!.ts_ms <= b.ts_ms) j++;
        const s = arr[j];
        return s && Math.abs(s.ts_ms - b.ts_ms) < 15_000 ? s.cpu_pct : null;
      });
    });
    return { names, data: [xs, ...ys] };
  }, [hist, cutoff]);

  const queue = queueHistory.filter((q) => q.ts * 1000 >= cutoff);
  const queueData = [queue.map((q) => q.ts), queue.map((q) => q.queued), queue.map((q) => q.running)];
  const hourly = ov.stats.hourly;

  return (
    <>
      <Toolbar range={range} onRange={onRange} source={source} onSource={onSource} sources={sources} />
      {samples.length < 2 ? <p className="empty-line py-10 text-center">No samples for {source} in this range yet.</p> : <HostCharts samples={samples} height={170} columns={3} />}

      <section className="mt-14">
        <SectionHeading index="02" label="Scheduler" title="Queue & throughput" />
        <div className="grid gap-4 lg:grid-cols-2">
          <Panel eyebrow="Queue depth" title={`${ov.queue.queued} queued · ${ov.queue.running} running`} description="Recorded from the live event stream since this console opened.">
            {queue.length < 2 ? (
              <p className="empty-line flex min-h-[196px] items-center justify-center">Waiting for queue events…</p>
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
          <Panel eyebrow="Throughput · 24h" title={`${ov.stats.runs_24h} runs · p50 job ${formatDuration(ov.stats.median_job_seconds)}`}>
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
    </>
  );
}

/** The metrics while they load: the toolbar, each chart's panel and space. */
function MetricsHold({ range, source }: { range: Range; source: string }) {
  const noop = () => undefined;
  return (
    <>
      <Toolbar range={range} onRange={noop} source={source} onSource={noop} sources={[source]} />
      <div className="grid grid-cols-1 gap-4 lg:grid-cols-2 2xl:grid-cols-3">
        {HOST_PANELS.map((p) => (
          <Panel key={p} eyebrow={p} title="—">
            <Hold height={196} />
          </Panel>
        ))}
      </div>
      <section className="mt-14">
        <SectionHeading index="02" label="Scheduler" title="Queue & throughput" />
        <div className="grid gap-4 lg:grid-cols-2">
          <Panel eyebrow="Queue depth" title="—" description="Recorded from the live event stream since this console opened.">
            <Hold height={196} />
          </Panel>
          <Panel eyebrow="Throughput · 24h" title="—">
            <Hold height={196} />
          </Panel>
        </div>
      </section>
    </>
  );
}
