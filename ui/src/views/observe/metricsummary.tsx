/**
 * Metrics summary: what each metric kiln keeps is and costs, Datadog's Metrics Summary. Its series
 * (its cardinality) by kind, each tag's values (what makes the series many), the samples it brings a
 * second and when it last did; the metrics of most series first. A metric's tags' values of most
 * series in a click; the metric itself in the explorer in another.
 */
import * as React from "react";
import { observeApi, type MetricSummary } from "@/api/observe";
import { Dialog, DialogContent } from "@/components/ui/dialog";
import { Hold, TableHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { PageIntro, SearchField } from "@/components/ui/misc";
import { useRes, type Res } from "@/lib/data";
import { relativeTime } from "@/lib/format";
import { Link } from "@/lib/router";
import { ObserveNav } from "./parts";

const LIMIT = 200;
const count = (n: number) => n.toLocaleString();

const summaryRes = (match: string): Res<{ matched: number; metrics: MetricSummary[] }> => ({
  key: `observe/metrics-summary/${match}`,
  fetch: (s) => observeApi.metricsSummary(match ? `*${match}*` : undefined, LIMIT, s),
  opts: { pollMs: 30_000 },
});
const oneRes = (name: string): Res<MetricSummary> => ({ key: `observe/metric-summary/${name}`, fetch: (s) => observeApi.metricSummary(name, s) });

function rate(m: MetricSummary): string {
  if (m.samples_per_s === null || m.samples_per_s === undefined) return "—";
  const r = m.samples_per_s;
  return r >= 100 ? `${count(Math.round(r))}/s` : `${r.toFixed(r >= 10 ? 1 : 2)}/s`;
}

export default function MetricsSummary() {
  const [filter, setFilter] = React.useState("");
  const [open, setOpen] = React.useState<string | null>(null);
  const match = filter.trim();
  return (
    <div className="container-console page">
      <title>Metrics summary · Observe · kiln</title>
      <PageIntro
        index={8}
        label="Observe"
        title="Metrics summary"
        lead="What each metric kiln keeps is and costs: its series, each tag's values (what makes the series many), and the samples it brings. The metrics of most series first."
      />
      <ObserveNav />
      <SearchField label="Find a metric" value={filter} onChange={setFilter} placeholder="a part of its name" className="mb-4 max-w-md" />
      <Loadable key={match} label="the metrics" fallback={<TableHold head={<Head />} rows={6} />}>
        <Table match={match} onOpen={setOpen} />
      </Loadable>
      {open && (
        <Dialog open onOpenChange={(o) => !o && setOpen(null)}>
          <DialogContent title={open} description="Its tags, the most values first, each with the values of most series.">
            <Loadable label="its tags" fallback={<Hold height={240} />}>
              <Tags name={open} />
            </Loadable>
          </DialogContent>
        </Dialog>
      )}
    </div>
  );
}

function Head() {
  return (
    <tr>
      <th>Metric</th>
      <th className="text-right">Series</th>
      <th className="hidden md:table-cell">Tags</th>
      <th className="hidden text-right sm:table-cell">Samples</th>
      <th className="hidden sm:table-cell">Last</th>
    </tr>
  );
}

/** A metric's tags in words: the four of most values, and how many more. */
function tagsText(m: MetricSummary): string {
  if (m.tags.length === 0) return "none";
  return (
    m.tags
      .slice(0, 4)
      .map((t) => `${t.key} ${count(t.values)}`)
      .join(" · ") + (m.tags.length > 4 ? ` · ${m.tags.length - 4} more` : "")
  );
}

function Table({ match, onOpen }: { match: string; onOpen: (name: string) => void }) {
  const { matched, metrics } = useRes(summaryRes(match)).data;
  if (metrics.length === 0) return <p className="empty-line py-12 text-center">{match ? "No metric of that name." : "No metrics yet: they arrive as hosts, CI and OTLP exporters report."}</p>;
  return (
    <>
      <div className="table-wrap">
        <table className="data-table">
          <thead>
            <Head />
          </thead>
          <tbody>
            {metrics.map((m) => (
              <tr key={m.name}>
                <td className="truncate-cell">
                  <Link to={`/observe/metrics?q=${encodeURIComponent(`avg:${m.name}{*}`)}`} className="block truncate font-mono text-[12px] text-[#e4e5e7] hover:text-white" title={m.name}>
                    {m.name}
                  </Link>
                  <p className="truncate text-[11px] text-[#8c8f98]">{m.kinds.map((k) => (m.kinds.length > 1 ? `${k.kind} ${count(k.series)}` : k.kind)).join(" · ")}</p>
                  <button type="button" className="block max-w-full truncate text-left text-[11px] text-[#c8cad0] md:hidden" onClick={() => onOpen(m.name)}>
                    {tagsText(m)}
                  </button>
                </td>
                <td className="text-right font-mono text-[12px] whitespace-nowrap">{count(m.series)}</td>
                <td className="hidden md:table-cell">
                  <button type="button" className="max-w-[26rem] truncate text-left text-[12px] text-[#c8cad0] hover:text-white" onClick={() => onOpen(m.name)} aria-label={`The tags of ${m.name}`}>
                    {tagsText(m)}
                  </button>
                </td>
                <td className="hidden text-right font-mono text-[12px] whitespace-nowrap sm:table-cell">{rate(m)}</td>
                <td className="hidden text-[12px] whitespace-nowrap text-[#8c8f98] sm:table-cell">{m.last_ms ? relativeTime(m.last_ms / 1000) : "—"}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {matched > metrics.length && <p className="mt-3 text-[12px] text-[#8c8f98]">{`The ${count(metrics.length)} of most series of ${count(matched)}; a part of a name finds the rest.`}</p>}
    </>
  );
}

function Tags({ name }: { name: string }) {
  const m = useRes(oneRes(name)).data;
  if (m.tags.length === 0) return <p className="empty-line py-8 text-center">Its series have no tags.</p>;
  return (
    <ul className="space-y-4">
      {m.tags.map((t) => (
        <li key={t.key}>
          <div className="flex items-baseline justify-between gap-3">
            <span className="font-mono text-[12px] text-[#e4e5e7]">{t.key}</span>
            <span className="text-[12px] text-[#8c8f98]">
              {count(t.values)} values · on {count(t.series)} of {count(m.series)} series
            </span>
          </div>
          <p className="mt-1 truncate font-mono text-[11px] text-[#8c8f98]">{(t.top ?? []).map((v) => `${v.value} (${count(v.series)})`).join(", ")}</p>
        </li>
      ))}
    </ul>
  );
}
