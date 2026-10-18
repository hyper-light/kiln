/**
 * A monitor: its state and each group's, what it watches drawn over the range with its
 * thresholds, and what it said (its events). `new` makes one, from a query (`?query=`, as the
 * metrics explorer sends it); a monitor's own page edits it, mutes it, deletes it.
 */
import * as React from "react";
import { BellOff, Bell, Pencil, Trash2 } from "lucide-react";
import { Link, useNavigate, useParams, useSearchParams } from "@/lib/router";
import { observeApi, type Monitor, type MonitorCheck, type MonitorIn } from "@/api/observe";
import { SERIES } from "@/components/charts/types";
import { Button } from "@/components/ui/button";
import { ConfirmDialog } from "@/components/ui/dialog";
import { Hold, RowsHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { Crumbs, Facts, Panel, SelectField, TextField } from "@/components/ui/misc";
import { toast } from "@/components/ui/toast";
import { useRes, type Res } from "@/lib/data";
import { errorMessage } from "@/lib/hooks";
import { relativeTime } from "@/lib/format";
import { invalidate } from "@/lib/resource";
import { Histogram, QueryChart, RangePicker, StateBadge, histogramRes, integrationsRes, logsOnly, monitorRes, useRange, type Mark, type Range } from "./parts";
import { Stream } from "./widgets";

const checkRes = (query: string): Res<MonitorCheck> => ({ key: `observe/monitor-check/${query}`, fetch: (s) => observeApi.validateMonitor(query, s) });

export default function MonitorPage() {
  const { id = "" } = useParams();
  const [params] = useSearchParams();
  return (
    <div className="container-console page max-w-[1100px]">
      {id === "new" ? (
        <>
          <title>New monitor · Observe · kiln</title>
          <header className="page-intro">
            <div className="min-w-0">
              <Crumbs index={8} items={[{ label: "Observe", to: "/observe" }, { label: "Monitors", to: "/observe/monitors" }, { label: "New" }]} />
              <h1>New monitor</h1>
            </div>
          </header>
          <MonitorForm initial={blank(params.get("query") ?? "")} />
        </>
      ) : (
        <Loadable key={id} label="the monitor" fallback={<Hold height={560} label="Loading the monitor" />}>
          <MonitorView id={id} />
        </Loadable>
      )}
    </div>
  );
}

const blank = (query: string): Monitor => ({
  id: "",
  name: "",
  query,
  message: "{{#is_alert}}{{monitor.name}} is alerting: {{value}}{{/is_alert}}\n{{#is_recovery}}{{monitor.name}} recovered{{/is_recovery}}",
  tags: [],
  priority: null,
  options: { thresholds: {}, notify_no_data: false },
  muted_until: null,
  created: 0,
  modified: 0,
  state: "OK",
  groups: [],
});

function MonitorView({ id }: { id: string }) {
  const res = useRes(monitorRes(id));
  const m = res.data;
  const [editing, setEditing] = React.useState(false);
  const [removing, setRemoving] = React.useState(false);
  const [range, setRange] = useRange("1d");
  const navigate = useNavigate();
  const now = Date.now();
  const isMuted = m.muted_until !== null && m.muted_until > now;
  const act = async (what: string, f: () => Promise<Monitor>) => {
    try {
      const after = await f();
      res.setData(() => after);
      invalidate("observe/monitors");
    } catch (e) {
      toast({ title: `Couldn't ${what}`, description: errorMessage(e), tone: "failed" });
    }
  };
  return (
    <>
      <title>{`${m.name} · Monitors · kiln`}</title>
      <header className="page-intro">
        <div className="min-w-0">
          <Crumbs index={8} items={[{ label: "Observe", to: "/observe" }, { label: "Monitors", to: "/observe/monitors" }, { label: m.id }]} />
          <h1>{m.name}</h1>
          <p className="lead flex flex-wrap items-center gap-3">
            <StateBadge state={m.state} muted={isMuted} />
            <code className="font-mono text-[12px] break-all text-[#a7aab3]">{m.query}</code>
          </p>
        </div>
        {!editing && (
          <div className="flex flex-wrap items-center gap-2">
            <Button variant="secondary" onClick={() => setEditing(true)}>
              <Pencil /> Edit
            </Button>
            {isMuted ? (
              <Button variant="ghost" onClick={() => void act("unmute it", () => observeApi.unmute(m.id))}>
                <Bell /> Unmute
              </Button>
            ) : (
              <Button variant="ghost" onClick={() => void act("mute it", () => observeApi.mute(m.id))}>
                <BellOff /> Mute
              </Button>
            )}
            <Button variant="ghost-danger" size="icon" aria-label="Delete the monitor" onClick={() => setRemoving(true)}>
              <Trash2 />
            </Button>
          </div>
        )}
      </header>
      {editing ? (
        <MonitorForm
          initial={m}
          onDone={(after) => {
            if (after) res.setData(() => after);
            setEditing(false);
          }}
        />
      ) : (
        <div className="space-y-10">
          <section aria-label="What it watches">
            <div className="mb-4">
              <RangePicker value={range} onChange={setRange} />
            </div>
            <Loadable key={m.query} label="what it watches" fallback={<Hold height={246} />}>
              <Watched m={m} range={range} />
            </Loadable>
          </section>
          <Groups m={m} />
          <Panel eyebrow="Settings" title="How it is evaluated and what it says">
            <Facts
              items={[
                { label: "Critical", value: m.options.thresholds.critical ?? "the query's" },
                { label: "Warning", value: m.options.thresholds.warning ?? "—" },
                {
                  label: "Recovers below",
                  value: [m.options.thresholds.critical_recovery, m.options.thresholds.warning_recovery].filter((v) => v !== null && v !== undefined).join(" / ") || "its thresholds",
                },
                { label: "No data", value: m.options.notify_no_data ? `after ${m.options.no_data_timeframe ?? "two windows'"} min` : "keeps its state" },
                { label: "Told again", value: m.options.renotify_interval ? `every ${m.options.renotify_interval} min` : "once" },
                { label: "Evaluated", value: `every ${m.options.every ?? 60} s${m.options.evaluation_delay ? `, ${m.options.evaluation_delay} s late` : ""}` },
              ]}
            />
            <pre className="mt-4 whitespace-pre-wrap rounded-md bg-[#0d0e10] p-3 font-mono text-[11.5px] text-[#c8cad0]">{m.message || "No message."}</pre>
          </Panel>
          <section aria-label="What it said">
            <h2 className="field-label">What it said, the last week</h2>
            <Loadable label="its events" fallback={<RowsHold rows={4} rowHeight={30} />}>
              <Stream query={`source:kiln.event @monitor_id:${m.id}`} range="7d" height={280} kind="events" />
            </Loadable>
          </section>
        </div>
      )}
      <ConfirmDialog
        open={removing}
        onOpenChange={setRemoving}
        title={`Delete ${m.name}?`}
        description="It stops being evaluated and says nothing more. What it said stays in the events."
        confirmLabel="Delete"
        danger
        onConfirm={async () => {
          try {
            await observeApi.deleteMonitor(m.id);
            invalidate("observe/monitors");
            navigate("/observe/monitors");
          } catch (e) {
            toast({ title: "Couldn't delete the monitor", description: errorMessage(e), tone: "failed" });
          }
        }}
      />
    </>
  );
}

/** What the monitor watches over the range, its thresholds across it. */
function Watched({ m, range }: { m: Monitor; range: Range }) {
  const check = useRes(checkRes(m.query)).data;
  if (!check.ok) return <p className="empty-line py-10 text-center">{check.error}</p>;
  if (check.kind === "logs") return <LogsWatched search={check.watched?.search ?? ""} range={range} />;
  const t = m.options.thresholds;
  // A change monitor's thresholds are of the change, not of the series drawn.
  const marks: Mark[] =
    check.kind === "metric"
      ? [{ label: "Critical", value: check.threshold ?? 0, color: SERIES.fail }, ...(t.warning !== null && t.warning !== undefined ? [{ label: "Warning", value: t.warning, color: SERIES.warn }] : [])]
      : [];
  return <QueryChart queries={[check.watched?.query ?? ""]} range={range} height={220} marks={marks} />;
}

function LogsWatched({ search, range }: { search: string; range: Range }) {
  const h = useRes(histogramRes(logsOnly(search), range)).data;
  return <Histogram h={h} range={range} height={200} />;
}

function Groups({ m }: { m: Monitor }) {
  if (m.groups.length === 0) return <p className="empty-line">Not evaluated yet: its first evaluation is within a minute of its making.</p>;
  const now = Date.now();
  return (
    <section aria-label="Groups">
      <h2 className="field-label">Each group</h2>
      <div className="table-wrap">
        <table className="data-table">
          <thead>
            <tr>
              <th>Group</th>
              <th>State</th>
              <th className="num">Value</th>
              <th>Since</th>
              <th>Last data</th>
            </tr>
          </thead>
          <tbody>
            {[...m.groups]
              .sort((a, b) => (a.state === b.state ? 0 : a.state === "OK" ? 1 : -1))
              .map((g) => (
                <tr key={g.group.map(([k, v]) => `${k}:${v}`).join(",")}>
                  <td className="cell-mono">{g.group.length ? g.group.map(([k, v]) => `${k}:${v}`).join(", ") : "*"}</td>
                  <td>
                    <StateBadge state={g.state} />
                  </td>
                  <td className="num cell-mono">{g.value === null ? "—" : String(Number(g.value.toPrecision(4)))}</td>
                  <td className="cell-mono">{relativeTime(g.since / 1000, now)}</td>
                  <td className="cell-mono">{g.last_data ? relativeTime(g.last_data / 1000, now) : "—"}</td>
                </tr>
              ))}
          </tbody>
        </table>
      </div>
    </section>
  );
}

const num = (s: string): number | null => (s.trim() === "" || !Number.isFinite(Number(s)) ? null : Number(s));
const str = (v: number | null | undefined) => (v === null || v === undefined ? "" : String(v));

/** A monitor's query, thresholds, message and options; made or changed on save. */
function MonitorForm({ initial, onDone }: { initial: Monitor; onDone?: (after: Monitor | null) => void }) {
  const [name, setName] = React.useState(initial.name);
  const [query, setQuery] = React.useState(initial.query);
  const [message, setMessage] = React.useState(initial.message);
  const [tags, setTags] = React.useState(initial.tags.join(", "));
  const [priority, setPriority] = React.useState(str(initial.priority));
  const t = initial.options.thresholds;
  const [warning, setWarning] = React.useState(str(t.warning));
  const [critRec, setCritRec] = React.useState(str(t.critical_recovery));
  const [warnRec, setWarnRec] = React.useState(str(t.warning_recovery));
  const [noData, setNoData] = React.useState(initial.options.notify_no_data);
  const [noDataMin, setNoDataMin] = React.useState(str(initial.options.no_data_timeframe));
  const [renotify, setRenotify] = React.useState(str(initial.options.renotify_interval));
  const [delay, setDelay] = React.useState(str(initial.options.evaluation_delay));
  const [every, setEvery] = React.useState(str(initial.options.every));
  const [check, setCheck] = React.useState<MonitorCheck | null>(null);
  const [busy, setBusy] = React.useState(false);
  const navigate = useNavigate();

  const body = (): MonitorIn => ({
    name: name.trim(),
    query: query.trim(),
    message,
    tags: tags
      .split(",")
      .map((x) => x.trim())
      .filter(Boolean),
    priority: num(priority),
    options: {
      thresholds: { warning: num(warning), critical_recovery: num(critRec), warning_recovery: num(warnRec) },
      notify_no_data: noData,
      no_data_timeframe: num(noDataMin),
      renotify_interval: num(renotify),
      evaluation_delay: num(delay),
      every: num(every),
    },
  });

  return (
    <form
      className="grid grid-cols-1 gap-8 lg:grid-cols-[minmax(0,1fr)_300px]"
      onSubmit={async (e) => {
        e.preventDefault();
        setBusy(true);
        try {
          const c = await observeApi.validateMonitor(query.trim());
          setCheck(c);
          if (!c.ok) return;
          const after = initial.id ? await observeApi.updateMonitor(initial.id, body()) : await observeApi.createMonitor(body());
          invalidate("observe/monitors");
          if (onDone) onDone(after);
          else navigate(`/observe/monitors/${encodeURIComponent(after.id)}`);
        } catch (err) {
          toast({ title: "Couldn't save the monitor", description: errorMessage(err), tone: "failed" });
        } finally {
          setBusy(false);
        }
      }}
    >
      <div className="space-y-5">
        <TextField label="Name" value={name} onChange={(e) => setName(e.target.value)} placeholder="Disk filling on {{host.name}}" required />
        <div>
          <label htmlFor="monitor-query" className="field-label">
            Query
          </label>
          <div className="field">
            <textarea
              id="monitor-query"
              rows={3}
              className="font-mono"
              value={query}
              onChange={(e) => {
                setQuery(e.target.value);
                setCheck(null);
              }}
              placeholder="avg(last_5m):max:system.disk.in_use{*} by {host,mount} > 0.9"
              spellCheck={false}
              required
            />
          </div>
          <div className="field-hint flex flex-wrap items-center gap-3">
            {check ? (
              check.ok ? (
                <span>
                  Alerts when {check.kind === "logs" ? "the count" : check.kind === "change" ? "the change" : "the value"} is {check.op} {check.threshold}.
                </span>
              ) : (
                <span className="text-[#e2c4c6]">{check.error}</span>
              )
            ) : (
              <span>Datadog&apos;s monitor syntax: a metric, its change, or logs counted.</span>
            )}
            <button type="button" className="text-link" onClick={() => void observeApi.validateMonitor(query.trim()).then(setCheck)}>
              Check
            </button>
          </div>
        </div>
        <div className="grid gap-3 sm:grid-cols-3">
          <TextField label="Warning at" inputMode="decimal" value={warning} onChange={(e) => setWarning(e.target.value)} />
          <TextField label="Recovers from critical at" inputMode="decimal" value={critRec} onChange={(e) => setCritRec(e.target.value)} />
          <TextField label="Recovers from warning at" inputMode="decimal" value={warnRec} onChange={(e) => setWarnRec(e.target.value)} />
        </div>
        <div>
          <label htmlFor="monitor-message" className="field-label">
            Message
          </label>
          <div className="field">
            <textarea id="monitor-message" rows={6} className="font-mono" value={message} onChange={(e) => setMessage(e.target.value)} />
          </div>
          <Loadable label="the integrations" fallback={null}>
            <Mentions />
          </Loadable>
        </div>
      </div>
      <aside className="space-y-4">
        <label className="flex items-center gap-2 text-[13px] text-[#c8cad0]">
          <input type="checkbox" className="check" checked={noData} onChange={(e) => setNoData(e.target.checked)} /> Say when data stops
        </label>
        {noData && <TextField label="After minutes without data" inputMode="numeric" value={noDataMin} onChange={(e) => setNoDataMin(e.target.value)} placeholder="two windows" />}
        <TextField label="Tell again every (minutes)" inputMode="numeric" value={renotify} onChange={(e) => setRenotify(e.target.value)} placeholder="once" />
        <TextField label="Evaluate every (seconds)" inputMode="numeric" value={every} onChange={(e) => setEvery(e.target.value)} placeholder="60" />
        <TextField label="Wait for late data (seconds)" inputMode="numeric" value={delay} onChange={(e) => setDelay(e.target.value)} placeholder="0" />
        <TextField label="Tags" value={tags} onChange={(e) => setTags(e.target.value)} placeholder="team:ci, service:api" />
        <SelectField label="Priority" value={priority} onChange={setPriority} options={[{ value: "", label: "None" }, ...[1, 2, 3, 4, 5].map((p) => ({ value: String(p), label: `P${p}` }))]} />
        <div className="flex gap-2 pt-2">
          <Button type="submit" disabled={busy || !name.trim() || !query.trim()}>
            {initial.id ? "Save" : "Create"}
          </Button>
          {onDone ? (
            <Button variant="ghost" onClick={() => onDone(null)}>
              Cancel
            </Button>
          ) : (
            <Button asChild variant="ghost">
              <Link to="/observe/monitors">Cancel</Link>
            </Button>
          )}
        </div>
      </aside>
    </form>
  );
}

/** Where a message can go: each integration's mention. */
function Mentions() {
  const all = useRes(integrationsRes).data;
  if (all.length === 0)
    return (
      <p className="field-hint">
        Template variables: {"{{value}}"}, {"{{threshold}}"}, {"{{monitor.name}}"}, {"{{host.name}}"}; sections {"{{#is_alert}}…{{/is_alert}}"}. To send it somewhere,{" "}
        <Link to="/observe/integrations" className="text-link">
          add an integration
        </Link>{" "}
        and mention it.
      </p>
    );
  return (
    <p className="field-hint">
      Mention where it goes: {all.map((i) => `@${i.kind}-${i.name}`).join(", ")}. Template variables: {"{{value}}"}, {"{{threshold}}"}, {"{{monitor.name}}"}, {"{{host.name}}"} (any tag of the
      group&apos;s), {"{{link}}"}.
    </p>
  );
}
