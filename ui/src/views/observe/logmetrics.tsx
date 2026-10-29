/**
 * Metrics from logs (docs/observe.md §8.3), Datadog's: a count of the records a search finds, or an
 * attribute's values as a distribution, grouped by attributes that become tags, a point every ten
 * seconds. The server counts each window once its records have come, and again when more come for
 * it within twenty minutes; the metrics outlive the logs.
 */
import * as React from "react";
import { LineChart, Pencil, Plus, Trash2, X } from "lucide-react";
import { Link } from "@/lib/router";
import { observeApi, type LogMetric, type LogMetricIn, type LogMetricTry } from "@/api/observe";
import { Button } from "@/components/ui/button";
import { ConfirmDialog, Dialog, DialogContent } from "@/components/ui/dialog";
import { TableHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { PageIntro, SelectField, TextField } from "@/components/ui/misc";
import { toast } from "@/components/ui/toast";
import { useRes } from "@/lib/data";
import { relativeTime } from "@/lib/format";
import { errorMessage } from "@/lib/hooks";
import { invalidate } from "@/lib/resource";
import { logMetricsRes, ObserveNav } from "./parts";

const count = (n: number) => n.toLocaleString();
const records = (n: number) => `${count(n)} ${n === 1 ? "record" : "records"}`;
const RESERVED = ["kiln.", "system.", "datadog."];
const validName = (n: string) => /^[A-Za-z][A-Za-z0-9_.]{0,199}$/.test(n) && !RESERVED.some((p) => n.startsWith(p));
const tagOf = (g: { path: string; tag_name?: string }) => (g.tag_name?.trim() ? g.tag_name.trim() : g.path.trim().replace(/^@/, ""));

/** What a metric computes, in words. */
function computes(m: LogMetricIn): string {
  if (m.compute.aggregation_type === "count") return "Count of records";
  return `Distribution of ${m.compute.path ?? ""}${m.compute.include_percentiles ? ", with percentiles" : ""}`;
}

/** The query that charts a metric in the explorer. */
function exploreQuery(m: LogMetricIn): string {
  const by = m.group_by.length > 0 ? ` by {${m.group_by.map(tagOf).join(",")}}` : "";
  if (m.compute.aggregation_type === "count") return `sum:${m.name}{*}${by}.as_count()`;
  return `${m.compute.include_percentiles ? "p95" : "avg"}:${m.name}{*}${by}`;
}

export default function LogMetrics() {
  const [editing, setEditing] = React.useState<LogMetric | "new" | null>(null);
  return (
    <div className="container-console page">
      <title>Metrics from logs · Observe · kiln</title>
      <PageIntro
        index={8}
        label="Observe"
        title="Metrics from logs"
        lead="A count of the records a search finds, or an attribute's values as a distribution, grouped by attributes that become tags: a point every ten seconds, counted on the server as records come, kept as long as metrics are."
        actions={
          <Button onClick={() => setEditing("new")}>
            <Plus /> New metric
          </Button>
        }
      />
      <ObserveNav />
      <Loadable label="the metrics from logs" fallback={<TableHold head={<Head />} rows={3} />}>
        <List onAdd={() => setEditing("new")} onEdit={setEditing} />
      </Loadable>
      {editing && <MetricDialog metric={editing === "new" ? null : editing} onClose={() => setEditing(null)} />}
    </div>
  );
}

function Head() {
  return (
    <tr>
      <th>Metric</th>
      <th className="hidden md:table-cell">Computes</th>
      <th className="hidden sm:table-cell">Grouped by</th>
      <th className="hidden sm:table-cell">Latest window</th>
      <th />
    </tr>
  );
}

function List({ onAdd, onEdit }: { onAdd: () => void; onEdit: (m: LogMetric) => void }) {
  const { log_metrics: all, counting } = useRes(logMetricsRes).data;
  const [removing, setRemoving] = React.useState<LogMetric | null>(null);
  const late = [
    counting.windows_recounted > 0 ? `${count(counting.windows_recounted)} counted again as late records came` : null,
    counting.windows_expired > 0 ? `${count(counting.windows_expired)} with records too late to count` : null,
  ].filter(Boolean);
  return (
    <>
      {all.length > 0 && (
        <p className="mb-4 text-[12.5px] text-[#8c8f98]">
          Each window is counted {Math.round(counting.settle_ms / 100) / 10} s after it ends, as long as records take to come here{late.length > 0 ? `; ${late.join(", ")}` : ""}.
          {counting.error && <span className="text-[#e2c4c6]"> Counting failed: {counting.error}</span>}
        </p>
      )}
      {all.length === 0 ? (
        <p className="empty-line py-12 text-center">
          None yet.{" "}
          <button type="button" className="text-link" onClick={onAdd}>
            Make one
          </button>{" "}
          to count errors by status code, or a latency&apos;s percentiles, from your logs.
        </p>
      ) : (
        <div className="table-wrap">
          <table className="data-table">
            <thead>
              <Head />
            </thead>
            <tbody>
              {all.map((m) => (
                <tr key={m.name}>
                  <td className="truncate-cell">
                    <div className="truncate font-mono text-[13px] text-[#e4e5e7]">{m.name}</div>
                    <p className="truncate font-mono text-[11px] text-[#8c8f98]">{m.filter.query.trim() || "every record"}</p>
                    {m.status && (
                      <p className="truncate text-[12px] text-[#8c8f98] sm:hidden">
                        {records(m.status.records)}, {relativeTime(m.status.window / 1000)}
                      </p>
                    )}
                  </td>
                  <td className="hidden whitespace-nowrap md:table-cell">{computes(m)}</td>
                  <td className="hidden sm:table-cell">
                    <div className="max-w-[16rem] truncate font-mono text-[12px]" title={m.group_by.map(tagOf).join(", ")}>
                      {m.group_by.length > 0 ? m.group_by.map(tagOf).join(", ") : "—"}
                    </div>
                  </td>
                  <td className="hidden whitespace-nowrap sm:table-cell">
                    {m.status ? (
                      <>
                        {relativeTime(m.status.window / 1000)}
                        <div className="text-[12px] text-[#8c8f98]">
                          {records(m.status.records)}, {count(m.status.groups)} {m.status.groups === 1 ? "group" : "groups"}
                          {m.status.overflowed > 0 ? `; ${count(m.status.overflowed)} past the 2,000 groups` : ""}
                        </div>
                      </>
                    ) : (
                      <span className="text-[#8c8f98]">None yet</span>
                    )}
                  </td>
                  <td className="w-1 whitespace-nowrap">
                    <Button variant="ghost" size="xs" asChild>
                      <Link to={`/observe/metrics?q=${encodeURIComponent(exploreQuery(m))}`} aria-label={`Chart ${m.name}`}>
                        <LineChart />
                      </Link>
                    </Button>
                    <Button variant="ghost" size="xs" aria-label={`Edit ${m.name}`} onClick={() => onEdit(m)}>
                      <Pencil />
                    </Button>
                    <Button variant="ghost-danger" size="xs" aria-label={`Remove ${m.name}`} onClick={() => setRemoving(m)}>
                      <Trash2 />
                    </Button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      <ConfirmDialog
        open={removing !== null}
        onOpenChange={(o) => !o && setRemoving(null)}
        title={`Remove ${removing?.name ?? ""}?`}
        description="It is counted no more; its points stay, until metrics' retention."
        confirmLabel="Remove"
        danger
        onConfirm={async () => {
          if (!removing) return;
          try {
            await observeApi.deleteLogMetric(removing.name);
            invalidate("observe/log_metrics");
          } catch (e) {
            toast({ title: "Couldn't remove it", description: errorMessage(e), tone: "failed" });
          }
        }}
      />
    </>
  );
}

interface Form {
  name: string;
  kind: "count" | "distribution";
  path: string;
  percentiles: boolean;
  query: string;
  groups: { path: string; tag: string }[];
}

const formOf = (m: LogMetric | null): Form => ({
  name: m?.name ?? "",
  kind: m?.compute.aggregation_type ?? "count",
  path: m?.compute.path ?? "@duration",
  percentiles: m?.compute.include_percentiles ?? true,
  query: m?.filter.query ?? "",
  groups: (m?.group_by ?? []).map((g) => ({ path: g.path, tag: g.tag_name ?? "" })),
});

const bodyOf = (f: Form): LogMetricIn => ({
  name: f.name.trim(),
  compute: f.kind === "count" ? { aggregation_type: "count" } : { aggregation_type: "distribution", path: f.path.trim(), include_percentiles: f.percentiles },
  filter: { query: f.query.trim() },
  group_by: f.groups.filter((g) => g.path.trim()).map((g) => (g.tag.trim() ? { path: g.path.trim(), tag_name: g.tag.trim() } : { path: g.path.trim() })),
});

function MetricDialog({ metric, onClose }: { metric: LogMetric | null; onClose: () => void }) {
  const [f, setF] = React.useState<Form>(() => formOf(metric));
  const [busy, setBusy] = React.useState(false);
  const [tried, setTried] = React.useState<LogMetricTry | string | null>(null);
  const set = (patch: Partial<Form>) => {
    setF((old) => ({ ...old, ...patch }));
    setTried(null);
  };
  const setGroup = (i: number, patch: Partial<Form["groups"][number]>) => set({ groups: f.groups.map((g, j) => (j === i ? { ...g, ...patch } : g)) });
  const nameOk = validName(f.name.trim());
  const pathOk = f.kind === "count" || /^@.+/.test(f.path.trim());
  const ok = nameOk && pathOk && f.groups.length <= 8;
  const save = async () => {
    setBusy(true);
    try {
      if (metric) await observeApi.updateLogMetric(metric.name, bodyOf(f));
      else await observeApi.createLogMetric(bodyOf(f));
      invalidate("observe/log_metrics");
      onClose();
    } catch (err) {
      toast({ title: "Couldn't save it", description: errorMessage(err), tone: "failed" });
    } finally {
      setBusy(false);
    }
  };
  const tryIt = async () => {
    setBusy(true);
    try {
      setTried(await observeApi.tryLogMetric(bodyOf(f)));
    } catch (err) {
      setTried(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };
  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent
        title={metric ? `Edit ${metric.name}` : "New metric from logs"}
        description={
          metric ? "A change counts from the next ten seconds on; what it computes stays (a new metric changes that)." : "Counted from the next ten seconds on, every ten seconds of records' time."
        }
      >
        <form
          className="space-y-4"
          onSubmit={(e) => {
            e.preventDefault();
            if (ok) void save();
          }}
        >
          <TextField
            label="Name"
            value={f.name}
            readOnly={metric !== null}
            onChange={(e) => set({ name: e.target.value })}
            placeholder="checkout.errors"
            spellCheck={false}
            hint={f.name && !nameOk ? "A letter, then letters, digits, _ and . (kiln.*, system.* and datadog.* are kiln's)" : undefined}
          />
          <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
            {metric ? (
              <TextField label="Computes" value={computes(bodyOf(f))} readOnly />
            ) : (
              <SelectField
                label="Computes"
                value={f.kind}
                onChange={(v) => set({ kind: v === "distribution" ? "distribution" : "count" })}
                options={[
                  { value: "count", label: "Count of records" },
                  { value: "distribution", label: "Distribution of an attribute" },
                ]}
              />
            )}
            {f.kind === "distribution" && !metric && (
              <TextField
                label="Of the attribute"
                value={f.path}
                onChange={(e) => set({ path: e.target.value })}
                placeholder="@duration"
                spellCheck={false}
                hint={!pathOk ? "An attribute: @name" : undefined}
              />
            )}
          </div>
          {f.kind === "distribution" && (
            <label className="flex items-center gap-2 text-[13px] text-[#c8cad0]">
              <input type="checkbox" className="check" checked={f.percentiles} onChange={(e) => set({ percentiles: e.target.checked })} /> Keep percentiles (p50, p95, p99, any)
            </label>
          )}
          <TextField
            label="Of the records"
            value={f.query}
            onChange={(e) => set({ query: e.target.value })}
            placeholder="service:checkout status:error"
            spellCheck={false}
            hint="A log search; empty, every record."
          />
          <div>
            <div className="field-label">Grouped by</div>
            <div className="space-y-2">
              {f.groups.map((g, i) => (
                <div key={i} className="flex items-end gap-2">
                  <TextField
                    className="min-w-0 flex-1"
                    label={`Attribute ${i + 1}`}
                    value={g.path}
                    onChange={(e) => setGroup(i, { path: e.target.value })}
                    placeholder="@http.status_code"
                    spellCheck={false}
                  />
                  <TextField
                    className="min-w-0 flex-1"
                    label="As the tag"
                    value={g.tag}
                    onChange={(e) => setGroup(i, { tag: e.target.value })}
                    placeholder={g.path.trim().replace(/^@/, "") || "status_code"}
                    spellCheck={false}
                  />
                  <Button variant="ghost" size="xs" className="mb-1.5" aria-label={`Group by ${g.path || "it"} no more`} onClick={() => set({ groups: f.groups.filter((_, j) => j !== i) })}>
                    <X />
                  </Button>
                </div>
              ))}
            </div>
            {f.groups.length < 8 && (
              <Button variant="ghost" size="xs" className="mt-2" onClick={() => set({ groups: [...f.groups, { path: "", tag: "" }] })}>
                <Plus /> Group by an attribute
              </Button>
            )}
          </div>
          {tried && <Tried tried={tried} />}
          <div className="flex justify-end gap-2">
            <Button variant="ghost" onClick={onClose}>
              Cancel
            </Button>
            <Button variant="ghost" disabled={!ok || busy} onClick={() => void tryIt()}>
              Try it
            </Button>
            <Button type="submit" disabled={!ok || busy}>
              {metric ? "Save" : "Make it"}
            </Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  );
}

/** What a definition counts of the last fifteen minutes' records: each group's total, the most first. */
function Tried({ tried }: { tried: LogMetricTry | string }) {
  if (typeof tried === "string")
    return (
      <p className="field-hint text-[#e2c4c6]" role="status">
        {tried}
      </p>
    );
  // A count's groups by its own series (named `preview`); a distribution's by its count's.
  const counted = tried.series.filter((s) => s.metric === "preview" || s.metric === "preview.count");
  const groups = counted
    .map((s) => ({
      tags: Object.entries(s.tags)
        .map(([k, v]) => `${k}:${v}`)
        .join(", "),
      total: s.points.reduce((a, [, v]) => a + v, 0),
    }))
    .sort((a, b) => b.total - a.total);
  return (
    <div role="status" className="space-y-2">
      <p className="field-hint">
        Of the last fifteen minutes: {records(tried.records)}, {count(groups.length)} {groups.length === 1 ? "group" : "groups"}.
      </p>
      {groups.length > 0 && (
        <div className="table-wrap max-h-48 overflow-y-auto">
          <table className="data-table">
            <tbody>
              {groups.slice(0, 20).map((g) => (
                <tr key={g.tags}>
                  <td className="truncate-cell font-mono text-[12px]">{g.tags || "every record"}</td>
                  <td className="num cell-mono w-1">{count(g.total)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}
