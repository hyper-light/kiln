/**
 * A container (docs/observe.md §10.10): whether it runs, its image, engine, host and pod; its CPU
 * and memory against its limits, its throttling, processes and uptime; its CPU (with its limit),
 * memory (in use, its working set, its limit), throttling and I/O over a range, each line drawn as
 * the source that reported it names it (kiln's check, or an OpenTelemetry collector: §10.11); its
 * tags; its logs a click away.
 */
import * as React from "react";
import { observeApi, type ChartLine, type ContainerDetail } from "@/api/observe";
import { Button } from "@/components/ui/button";
import { Hold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { Crumbs, Facts, Panel } from "@/components/ui/misc";
import { useRes, type Res } from "@/lib/data";
import { formatBytes, formatDuration, relativeTime } from "@/lib/format";
import { Link, useParams } from "@/lib/router";
import { OfLimit, cores, coresText, imageOf, nameOf } from "./containers";
import { QueryChart, RangePicker, compact, useRange, type Range } from "./parts";

const containerRes = (id: string): Res<ContainerDetail> => ({ key: `observe/containers/one/${id}`, fetch: (s) => observeApi.container(id, s), opts: { pollMs: 15_000 } });

export default function ContainerPage() {
  const { id = "" } = useParams();
  const [range, setRange] = useRange();
  return (
    <div className="container-console page">
      <Loadable key={id} label="the container" fallback={<Hold height={480} />}>
        <ContainerView id={id} range={range} onRange={setRange} />
      </Loadable>
    </div>
  );
}

function ContainerView({ id, range, onRange }: { id: string; range: Range; onRange: (r: Range) => void }) {
  const c = useRes(containerRes(id)).data;
  const f = c.figures;
  const pod = c.kube.pod ? `${c.kube.namespace ? `${c.kube.namespace}/` : ""}${c.kube.pod}` : null;
  const memory = f.working_set_bytes ?? f.memory_bytes;
  return (
    <>
      <title>{`${nameOf(c)} · Containers · Observe · kiln`}</title>
      <header className="page-intro">
        <div className="min-w-0">
          <Crumbs index={8} items={[{ label: "Observe", to: "/observe" }, { label: "Containers", to: "/observe/containers" }, { label: nameOf(c) }]} />
          <h1 className="truncate font-mono">{nameOf(c)}</h1>
          <p className="lead max-w-3xl text-[13px]">
            <span className="status whitespace-nowrap">
              <i className="dot" data-status={c.state === "running" ? "succeeded" : "cancelled"} aria-hidden="true" />
              <span className="status-text" data-status={c.state === "running" ? "succeeded" : "cancelled"}>
                {c.state === "running" ? "Running" : "Stopped"}
              </span>
            </span>
            <span className="text-[#8c8f98]"> — its last sample {relativeTime(c.last_reported / 1000)}</span>
            <span className="block font-mono text-[12px] text-[#8c8f98] [overflow-wrap:anywhere]">{c.id}</span>
          </p>
        </div>
        <div className="flex flex-wrap gap-2">
          <Button variant="secondary" asChild>
            <Link to={`/observe/logs?q=${encodeURIComponent(`@container.id:${c.id} OR container_id:${c.id}`)}`}>Its logs</Link>
          </Button>
        </div>
      </header>
      <Facts
        className="mb-8"
        items={[
          { label: "CPU (cores)", value: <OfLimit use={cores(f.cpu_nanocores)} limit={cores(f.cpu_limit_nanocores)} fmt={coresText} /> },
          { label: "CPU limit", value: f.cpu_limit_nanocores !== null ? `${compact(f.cpu_limit_nanocores / 1e9)} cores` : "None" },
          { label: "Memory (working set)", value: <OfLimit use={memory} limit={f.memory_limit_bytes} fmt={(v) => formatBytes(v)} /> },
          { label: "Memory limit", value: f.memory_limit_bytes !== null ? formatBytes(f.memory_limit_bytes) : "None" },
          { label: "Throttled", value: f.cpu_throttled_ns !== null ? `${compact(f.cpu_throttled_ns / 1e6)} ms/s` : "—", tone: (f.cpu_throttled_ns ?? 0) > 1e8 ? "fail" : undefined },
          { label: "Processes", value: f.pids !== null ? compact(f.pids) : "—" },
          { label: "I/O", value: f.io_read_bytes !== null || f.io_write_bytes !== null ? `${formatBytes(f.io_read_bytes ?? 0)}/s in · ${formatBytes(f.io_write_bytes ?? 0)}/s out` : "—" },
          { label: "Up", value: f.uptime_seconds !== null ? formatDuration(f.uptime_seconds) : "—" },
        ]}
      />
      <div className="mb-5">
        <RangePicker value={range} onChange={onRange} />
      </div>
      <div className="mb-8 grid grid-cols-1 gap-4 lg:grid-cols-2">
        <Panel title="CPU" description="Cores in use, and its limit.">
          <Lines label="its CPU" lines={c.charts.cpu} range={range} unit="cores" />
        </Panel>
        <Panel title="Memory" description="In use, its working set (what an out-of-memory kill reads), and its limit.">
          <Lines label="its memory" lines={c.charts.memory} range={range} unit="B" />
        </Panel>
        <Panel title="Throttling" description="Time its quota held it back, a second.">
          <Lines label="its throttling" lines={c.charts.throttled} range={range} unit="ms/s" />
        </Panel>
        <Panel title="I/O" description="Bytes read and written, a second.">
          <Lines label="its I/O" lines={c.charts.io} range={range} unit="B/s" />
        </Panel>
      </div>
      <div className="mb-8 grid grid-cols-1 gap-4 lg:grid-cols-2">
        <Panel title="Where it runs">
          <dl className="space-y-1.5 text-[12.5px]">
            {(
              [
                [
                  "Host",
                  c.host ? (
                    <Link to={`/observe/hosts/${encodeURIComponent(c.host)}`} className="text-link font-mono">
                      {c.host}
                    </Link>
                  ) : null,
                ],
                ["Image", imageOf(c) || null],
                ["Engine", c.runtime],
                ["Pod", pod],
                ["Kubernetes container", c.kube.container],
                ["QoS class", c.kube.qos],
                ["Compose", c.compose.service ? `${c.compose.project ? `${c.compose.project}/` : ""}${c.compose.service}` : null],
                ["Series", String(c.series)],
              ] as [string, React.ReactNode][]
            )
              .filter(([, v]) => v)
              .map(([k, v]) => (
                <div key={k} className="flex min-w-0 items-baseline gap-3">
                  <dt className="w-36 shrink-0 text-[#8c8f98]">{k}</dt>
                  <dd className="min-w-0 [overflow-wrap:anywhere]">{v}</dd>
                </div>
              ))}
          </dl>
        </Panel>
        <Panel title="Tags" description="What its series carry.">
          <div className="flex flex-wrap gap-1.5">
            {c.tags.map((t) => (
              <span key={t} className="chip font-mono">
                {t}
              </span>
            ))}
          </div>
        </Panel>
      </div>
    </>
  );
}

/** A chart of a container's lines, each as the first of its sources that reported it draws it; none reported, said so. */
function Lines({ label, lines, range, unit }: { label: string; lines: ChartLine[]; range: Range; unit: string }) {
  const queries = React.useMemo(() => lines.map((l) => l.query), [lines]);
  const names = React.useMemo(() => lines.map((l) => l.name), [lines]);
  if (lines.length === 0) return <p className="empty-line flex min-h-[180px] items-center justify-center">None of its sources reports this.</p>;
  return (
    <Loadable label={label} fallback={<Hold height={180} />}>
      <QueryChart queries={queries} names={names} range={range} unit={unit} />
    </Loadable>
  );
}
