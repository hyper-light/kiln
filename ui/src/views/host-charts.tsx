import * as React from "react";
import type { HostSample } from "@/api/types";
import { Chart } from "@/components/charts/chart";
import { SERIES } from "@/components/charts/types";
import { Panel } from "@/components/ui/misc";
import { formatBytes, formatPct, formatRate } from "@/lib/format";

/** Derive per-second network rates from cumulative counters (resets → gap). */
export function netRates(samples: HostSample[]) {
  const rx: (number | null)[] = [];
  const tx: (number | null)[] = [];
  for (let i = 0; i < samples.length; i++) {
    const cur = samples[i]!;
    const prev = samples[i - 1];
    if (!prev) {
      rx.push(null);
      tx.push(null);
      continue;
    }
    const dt = (cur.ts_ms - prev.ts_ms) / 1000;
    const drx = cur.net_rx - prev.net_rx;
    const dtx = cur.net_tx - prev.net_tx;
    rx.push(dt > 0 && drx >= 0 ? drx / dt : null);
    tx.push(dt > 0 && dtx >= 0 ? dtx / dt : null);
  }
  return { rx, tx };
}

export function useHostSeries(samples: HostSample[]) {
  return React.useMemo(() => {
    const xs = samples.map((s) => s.ts_ms / 1000);
    const { rx, tx } = netRates(samples);
    return {
      cpu: [xs, samples.map((s) => s.cpu_pct)],
      mem: [xs, samples.map((s) => s.mem_used), samples.map((s) => s.mem_total)],
      load: [xs, samples.map((s) => s.load1), samples.map((s) => s.cpus)],
      net: [xs, rx, tx],
      disk: [xs, samples.map((s) => (s.disk_total ? (s.disk_used / s.disk_total) * 100 : null))],
    };
  }, [samples]);
}

export function HostCharts({ samples, height = 150, columns = 2 }: { samples: HostSample[]; height?: number; columns?: 1 | 2 | 3 }) {
  const d = useHostSeries(samples);
  const last = samples[samples.length - 1];
  const grid = columns === 1 ? "grid-cols-1" : columns === 2 ? "grid-cols-1 lg:grid-cols-2" : "grid-cols-1 lg:grid-cols-2 2xl:grid-cols-3";
  return (
    <div className={`grid gap-4 ${grid}`}>
      <Panel eyebrow="CPU" title={last ? formatPct(last.cpu_pct, 1) : "—"} description={last ? `${last.cpus} cores` : undefined}>
        <Chart ariaLabel="CPU percent" data={d.cpu} height={height} yRange={[0, 100]} yFormat={(v) => `${Math.round(v)}%`} series={[{ label: "CPU", color: SERIES.violet, fill: true }]} />
      </Panel>
      <Panel eyebrow="Memory" title={last ? `${formatBytes(last.mem_used)} / ${formatBytes(last.mem_total)}` : "—"}>
        <Chart
          ariaLabel="Memory used"
          data={d.mem}
          height={height}
          yFormat={(v) => formatBytes(v, 0)}
          series={[
            { label: "Used", color: SERIES.blue, fill: true },
            { label: "Total", color: SERIES.grey, dash: [2, 4], width: 1 },
          ]}
        />
      </Panel>
      <Panel eyebrow="Load (1m)" title={last ? last.load1.toFixed(2) : "—"} description={last ? `${last.procs} processes` : undefined}>
        <Chart
          ariaLabel="Load average"
          data={d.load}
          height={height}
          yFormat={(v) => v.toFixed(1)}
          series={[
            { label: "Load", color: SERIES.sand, fill: true },
            { label: "Cores", color: SERIES.grey, dash: [2, 4], width: 1 },
          ]}
        />
      </Panel>
      <Panel eyebrow="Network" title={last ? `↓ ${formatRate(d.net[1]?.[d.net[1].length - 1] ?? null)}  ↑ ${formatRate(d.net[2]?.[d.net[2].length - 1] ?? null)}` : "—"}>
        <Chart
          ariaLabel="Network rate"
          data={d.net}
          height={height}
          yFormat={(v) => formatBytes(v, 0)}
          series={[
            { label: "Rx/s", color: SERIES.teal, fill: true },
            { label: "Tx/s", color: SERIES.rose },
          ]}
        />
      </Panel>
      {columns !== 2 && (
        <Panel eyebrow="Disk" title={last ? `${formatPct(last.disk_total ? (last.disk_used / last.disk_total) * 100 : 0)} used` : "—"} description={last ? `${formatBytes(last.disk_used)} of ${formatBytes(last.disk_total)}` : undefined}>
          <Chart ariaLabel="Disk used percent" data={d.disk} height={height} yRange={[0, 100]} yFormat={(v) => `${Math.round(v)}%`} series={[{ label: "Disk", color: SERIES.pending, fill: true }]} />
        </Panel>
      )}
    </div>
  );
}
