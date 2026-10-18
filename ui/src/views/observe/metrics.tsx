/**
 * The metrics explorer: a query (docs/observe.md §6), its series over a range and each one's
 * figures. With no query yet, every metric kiln keeps, to start from. A query goes from here onto
 * a dashboard, or into a monitor, in a step.
 */
import * as React from "react";
import { Link, useNavigate } from "@/lib/router";
import { observeApi, type QueryAnswer } from "@/api/observe";
import { Chart } from "@/components/charts/chart";
import { SERIES } from "@/components/charts/types";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent } from "@/components/ui/dialog";
import { Hold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { PageIntro, SearchField, SelectField, TextField } from "@/components/ui/misc";
import { toast } from "@/components/ui/toast";
import { useRes, type Res } from "@/lib/data";
import { errorMessage } from "@/lib/hooks";
import { invalidate } from "@/lib/resource";
import { ObserveNav, PALETTE, RangePicker, aggregate, chartData, dashboardsRes, dataOf, formatValue, queriesRes, useParam, useRange, type Range } from "./parts";

const namesRes: Res<string[]> = { key: "observe/metric-names", fetch: (s) => observeApi.metrics(undefined, s).then((r) => r.metrics), opts: { pollMs: 300_000 } };

const EXAMPLES = ["avg:system.cpu.user{*} by {host}", "max:system.disk.in_use{*} by {host,mount}", "sum:kiln.ci.jobs.finished{*} by {status}.as_count()", "avg:kiln.ci.job.duration{*} by {repo}"];

export default function MetricsExplorer() {
  const [q, setQ] = useParam("q");
  const [range, setRange] = useRange();
  return (
    <div className="container-console page">
      <title>Metrics · Observe · kiln</title>
      <PageIntro index={8} label="Observe" title="Metrics" lead="Query any metric kiln keeps, over any range it keeps: every series, grouped and combined as the query says." />
      <ObserveNav />
      <QueryForm key={q} initial={q} onRun={setQ} />
      <div className="mb-6 flex flex-wrap items-center gap-3">
        <RangePicker value={range} onChange={setRange} />
        {q && (
          <div className="flex flex-wrap gap-2 sm:ml-auto">
            <AddToDashboard query={q} />
            <Button asChild variant="ghost">
              <Link to={`/observe/monitors/new?query=${encodeURIComponent(`avg(last_5m):${q} > 0`)}`}>Create monitor</Link>
            </Button>
          </div>
        )}
      </div>
      {q ? (
        <Loadable key={q} label="the query" fallback={<Hold height={420} />}>
          <Answer query={q} range={range} />
        </Loadable>
      ) : (
        <Loadable label="the metrics" fallback={<Hold height={420} />}>
          <Names onPick={(name) => setQ(`avg:${name}{*}`)} />
        </Loadable>
      )}
    </div>
  );
}

function QueryForm({ initial, onRun }: { initial: string; onRun: (q: string) => void }) {
  const [draft, setDraft] = React.useState(initial);
  return (
    <form
      className="mb-4 flex flex-wrap items-end gap-3"
      onSubmit={(e) => {
        e.preventDefault();
        onRun(draft.trim());
      }}
    >
      <div className="min-w-0 flex-1">
        <label htmlFor="observe-query" className="field-label">
          Query
        </label>
        <div className="field">
          <input
            id="observe-query"
            className="font-mono"
            list="observe-metric-names"
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
            placeholder={EXAMPLES[0]}
            spellCheck={false}
            autoComplete="off"
            autoCapitalize="off"
          />
        </div>
      </div>
      <Button type="submit" disabled={!draft.trim()}>
        Run
      </Button>
      <React.Suspense fallback={null}>
        <NameList />
      </React.Suspense>
    </form>
  );
}

/** Every metric as a query to start from, for the query field to suggest. */
function NameList() {
  const names = useRes(namesRes).data;
  return (
    <datalist id="observe-metric-names">
      {names.map((n) => (
        <option key={n} value={`avg:${n}{*}`} />
      ))}
    </datalist>
  );
}

/** With no query: every metric kiln keeps, found by name, and a few queries to start from. */
function Names({ onPick }: { onPick: (name: string) => void }) {
  const names = useRes(namesRes).data;
  const [filter, setFilter] = React.useState("");
  const shown = names.filter((n) => n.includes(filter.trim().toLowerCase())).slice(0, 300);
  return (
    <div className="grid grid-cols-1 gap-8 lg:grid-cols-[minmax(0,1fr)_320px]">
      <section aria-label="Metrics kiln keeps">
        <SearchField label="Find a metric" value={filter} onChange={setFilter} placeholder={`${names.length.toLocaleString()} metrics`} className="mb-3" />
        {shown.length === 0 ? (
          <p className="empty-line py-10 text-center">{names.length === 0 ? "No metrics yet: they arrive as hosts, CI and OTLP exporters report." : "No metric of that name."}</p>
        ) : (
          <ul className="row-list">
            {shown.map((n) => (
              <li key={n}>
                <button type="button" className="w-full py-2 text-left font-mono text-[12px] text-[#c8cad0] hover:text-white" onClick={() => onPick(n)}>
                  {n}
                </button>
              </li>
            ))}
          </ul>
        )}
      </section>
      <aside aria-label="Example queries">
        <p className="field-label">Start from</p>
        <ul className="space-y-2">
          {EXAMPLES.map((e) => (
            <li key={e}>
              <Link to={`/observe/metrics?q=${encodeURIComponent(e)}`} className="font-mono text-[11.5px] text-[#d1c6e6] underline decoration-[#635b73] underline-offset-4 hover:text-[#f1e7ff]">
                {e}
              </Link>
            </li>
          ))}
        </ul>
      </aside>
    </div>
  );
}

function Answer({ query, range }: { query: string; range: Range }) {
  const answers = useRes(queriesRes([query], range)).data;
  const a: QueryAnswer | undefined = answers[0];
  const { data, bands, drawn } = React.useMemo(() => chartData(answers), [answers]);
  if (!a) return null;
  if (a.scalar !== null) return <p className="py-10 text-center font-mono text-3xl text-[#e4e5e7]">{formatValue(a.scalar)}</p>;
  // Each series' figures are of its data: a forecast's values are not its last, nor its most.
  const rows = a.series.map((s, i) => ({
    label: drawn.find((d) => d.source === i && !d.forecast)?.label ?? "",
    color: PALETTE[i % PALETTE.length] ?? SERIES.grey,
    last: aggregate(dataOf(s), "last"),
    avg: aggregate(dataOf(s), "avg"),
    min: aggregate(dataOf(s), "min"),
    max: aggregate(dataOf(s), "max"),
  }));
  return (
    <>
      {rows.length === 0 ? (
        <p className="empty-line flex min-h-[280px] items-center justify-center">No series match in this range.</p>
      ) : (
        <Chart
          ariaLabel={query}
          data={data}
          height={280}
          legend={false}
          timeFormat={range === "7d" || range === "30d" ? "day" : "time"}
          yFormat={(v) => formatValue(v)}
          series={drawn.map((d) => ({ label: d.label, color: PALETTE[d.source % PALETTE.length] ?? SERIES.grey, ...(d.forecast ? { dash: [6, 4] } : {}) }))}
          bands={bands}
        />
      )}
      <p className="mt-3 font-mono text-[10px] text-[#7b7e88]">
        {a.series.length.toLocaleString()} series · {a.scanned.series.toLocaleString()} read, {a.scanned.samples.toLocaleString()} samples · {a.took_ms.toFixed(1)} ms · every {formatStep(a.grid.step)}{" "}
        · as of entry {a.applied.toLocaleString()}
      </p>
      {rows.length > 0 && (
        <div className="table-wrap mt-6">
          <table className="data-table">
            <thead>
              <tr>
                <th>Series</th>
                <th className="num">Last</th>
                <th className="num">Avg</th>
                <th className="num">Min</th>
                <th className="num">Max</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((r) => (
                <tr key={r.label}>
                  <td className="cell-mono">
                    <span className="mr-2 inline-block size-2 rounded-full align-middle" style={{ background: r.color }} aria-hidden="true" />
                    {r.label}
                  </td>
                  <td className="num cell-mono">{formatValue(r.last)}</td>
                  <td className="num cell-mono">{formatValue(r.avg)}</td>
                  <td className="num cell-mono">{formatValue(r.min)}</td>
                  <td className="num cell-mono">{formatValue(r.max)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </>
  );
}

const formatStep = (ms: number) => (ms % 86_400_000 === 0 ? `${ms / 86_400_000} d` : ms % 3_600_000 === 0 ? `${ms / 3_600_000} h` : ms % 60_000 === 0 ? `${ms / 60_000} min` : `${ms / 1000} s`);

const NEW = "new";

/** Puts the query on a dashboard of the user's (or a new one) as a timeseries, and goes there. */
function AddToDashboard({ query }: { query: string }) {
  const [open, setOpen] = React.useState(false);
  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <Button variant="secondary" onClick={() => setOpen(true)}>
        Add to dashboard
      </Button>
      {open && (
        <DialogContent title="Add to a dashboard" description="As a timeseries of this query, below what the dashboard has.">
          <Loadable label="the dashboards">
            <AddForm query={query} onDone={() => setOpen(false)} />
          </Loadable>
        </DialogContent>
      )}
    </Dialog>
  );
}

function AddForm({ query, onDone }: { query: string; onDone: () => void }) {
  const mine = useRes(dashboardsRes).data.filter((d) => !d.builtin);
  const [target, setTarget] = React.useState(mine[0]?.id ?? NEW);
  const [title, setTitle] = React.useState(query);
  const [name, setName] = React.useState("New dashboard");
  const [busy, setBusy] = React.useState(false);
  const navigate = useNavigate();
  return (
    <form
      className="space-y-4"
      onSubmit={async (e) => {
        e.preventDefault();
        setBusy(true);
        try {
          const widget = { id: "", title, kind: "timeseries" as const, queries: [query], search: "", text: "", layout: { x: 0, y: 0, w: 6, h: 4 }, options: null };
          let id: string;
          if (target === NEW) {
            id = (await observeApi.createDashboard({ title: name.trim() || "New dashboard", description: "", template_variables: [], widgets: [widget] })).id;
          } else {
            const d = await observeApi.dashboard(target);
            const below = d.widgets.reduce((m, w) => Math.max(m, w.layout.y + w.layout.h), 0);
            id = (await observeApi.updateDashboard({ ...d, widgets: [...d.widgets, { ...widget, layout: { ...widget.layout, y: below } }] })).id;
          }
          invalidate("observe/dashboard");
          onDone();
          navigate(`/observe/dashboards/${encodeURIComponent(id)}`);
        } catch (err) {
          toast({ title: "Couldn't add it", description: errorMessage(err), tone: "failed" });
        } finally {
          setBusy(false);
        }
      }}
    >
      <SelectField label="Dashboard" value={target} onChange={setTarget} options={[...mine.map((d) => ({ value: d.id, label: d.title })), { value: NEW, label: "A new dashboard" }]} />
      {target === NEW && <TextField label="New dashboard's title" value={name} onChange={(e) => setName(e.target.value)} />}
      <TextField label="Widget's title" value={title} onChange={(e) => setTitle(e.target.value)} />
      <div className="flex justify-end">
        <Button type="submit" disabled={busy}>
          Add
        </Button>
      </div>
    </form>
  );
}
