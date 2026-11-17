/**
 * A monitor: its state and each group's, what it watches drawn over the range with its
 * thresholds (a composite: the monitors it reads, each one's state), and what it said (its events).
 * `new` makes one, from a query (`?query=`, as the metrics explorer sends it), or a composite
 * (`?type=composite`); a monitor's own page edits it, mutes it, deletes it.
 */
import * as React from "react";
import { BellOff, Bell, ChevronDown, Pencil, Trash2 } from "lucide-react";
import { Link, useNavigate, useParams, useSearchParams } from "@/lib/router";
import { observeApi, type CompositeOf, type Monitor, type MonitorCheck, type MonitorIn, type MonitorOptions, type NotificationPreset, type OnMissingData, type RenotifyStatus } from "@/api/observe";
import { SERIES } from "@/components/charts/types";
import { Button } from "@/components/ui/button";
import { ConfirmDialog } from "@/components/ui/dialog";
import { Hold, RowsHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { Crumbs, Facts, Panel, SelectField, TextField } from "@/components/ui/misc";
import { Segmented } from "@/components/ui/tabs";
import { toast } from "@/components/ui/toast";
import { sendingProps, useAction } from "@/lib/action";
import { useRes, type Res } from "@/lib/data";
import { errorMessage } from "@/lib/hooks";
import { relativeTime } from "@/lib/format";
import { invalidate } from "@/lib/resource";
import {
  CHECK_WORDS,
  Histogram,
  HeldFields,
  QueryChart,
  RangePicker,
  StateBadge,
  histogramRes,
  integrationsRes,
  logsOnly,
  monitorRes,
  monitorsRes,
  spanHistogramRes,
  useOpening,
  useRange,
  type Range,
  type Threshold,
} from "./parts";
import { CheckRuns, Stream } from "./widgets";

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
          <MonitorForm initial={blank(params.get("query") ?? "", params.get("type"))} />
        </>
      ) : (
        <Loadable key={id} label="the monitor" fallback={<Hold height={560} label="Loading the monitor" />}>
          <MonitorView id={id} />
        </Loadable>
      )}
    </div>
  );
}

const COMPOSITE = "composite";
/** A composite's monitors' letters, as Datadog's form gives them: ten at the most. */
const LETTERS = "abcdefghij";

/** A new monitor's message: a query monitor's says its value, a service check's its run's message;
 * a composite has none. */
const MESSAGE = {
  query: "{{#is_alert}}{{monitor.name}} is alerting: {{value}}{{/is_alert}}\n{{#is_recovery}}{{monitor.name}} recovered{{/is_recovery}}",
  check: "{{#is_alert}}{{monitor.name}}: {{check_message}}{{/is_alert}}\n{{#is_recovery}}{{monitor.name}} recovered{{/is_recovery}}",
  composite: "{{#is_alert}}{{monitor.name}} is alerting{{/is_alert}}\n{{#is_recovery}}{{monitor.name}} recovered{{/is_recovery}}",
};

/** Datadog's type of a service check monitor, whose query starts with its check's name, quoted. */
const SERVICE_CHECK = "service check";
const isCheckQuery = (q: string) => /^\s*"/.test(q);
/** A service check monitor's group's value: its latest run's status. */
const CHECK_CODES = ["OK", "WARNING", "CRITICAL", "UNKNOWN"] as const;

/** A monitor of logs, spans, events or Error Tracking's, by its query: what Datadog's `on_missing_data`
 * and `group_retention_duration` are of. */
const RECORDS = /^\s*(logs|spans|trace-analytics|events|error-tracking)\(/;
/** The states Datadog tells again, as a monitor's states name them; unless it says, alert and no data. */
const RENOTIFY: { value: RenotifyStatus; state: string }[] = [
  { value: "alert", state: "ALERT" },
  { value: "warn", state: "WARN" },
  { value: "no data", state: "NO DATA" },
];
const RENOTIFY_DEFAULT: RenotifyStatus[] = ["alert", "no data"];
/** `timeout_h`'s hours: Datadog's 1 to 24, or never. */
const TIMEOUT_HOURS = Array.from({ length: 24 }, (_, i) => i + 1);
/** Datadog's `on_missing_data`, its default unsaid. */
const MISSING: { value: "" | Exclude<OnMissingData, "default">; label: string }[] = [
  { value: "", label: "Counts as 0, or keeps its state" },
  { value: "show_no_data", label: "Shows NO DATA" },
  { value: "show_and_notify_no_data", label: "Shows NO DATA, told" },
  { value: "resolve", label: "Resolves" },
];
/** Datadog's `notification_preset_name`, `show_all` unsaid: kiln's notices show no query, handles or
 * snapshot, so what the others drop is their footer's link back. */
const PRESETS: { value: "" | Exclude<NotificationPreset, "show_all">; label: string }[] = [
  { value: "", label: "Everything" },
  { value: "hide_query", label: "Not its query" },
  { value: "hide_handles", label: "Not its handles" },
  { value: "hide_query_and_handles", label: "Neither query nor handles" },
  { value: "hide_handles_and_footer", label: "No handles, no footer" },
  { value: "show_only_snapshot", label: "Only a snapshot" },
  { value: "hide_all", label: "Nothing more" },
];
const sameSet = <T,>(a: T[], b: T[]) => a.length === b.length && a.every((x) => b.includes(x));

/** How a monitor is told again, in a line. */
const toldAgain = (o: MonitorOptions) =>
  o.renotify_interval
    ? [`every ${o.renotify_interval} min`, o.renotify_statuses?.length && `in ${o.renotify_statuses.join(", ")}`, o.renotify_occurrences && `${o.renotify_occurrences}× at most`].filter(Boolean).join(" · ")
    : "once";

/** What its notice shows besides its message, where it differs, in a line. */
const noticeSaid = (o: MonitorOptions) =>
  [
    o.include_tags === false && "no group in its title",
    o.notification_preset_name && o.notification_preset_name !== "show_all" && PRESETS.find((p) => p.value === o.notification_preset_name)?.label,
    o.enable_logs_sample && "its latest logs",
    o.notify_audit && "its changes told",
  ]
    .filter(Boolean)
    .join(" · ");

const blank = (query: string, type: string | null): Monitor => ({
  id: "",
  name: "",
  type: type === COMPOSITE ? COMPOSITE : null,
  query,
  message: type === COMPOSITE ? MESSAGE.composite : isCheckQuery(query) ? MESSAGE.check : MESSAGE.query,
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
  // Muted or unmuted at once (W21-R15), and put back if the server refuses; muted without an end,
  // as the server mutes it, until its answer says. The next press waits for this one's answer, so
  // an answer never undoes a later press.
  const [act, muting] = useAction(async (what: string, shown: Partial<Monitor>, f: () => Promise<Monitor>) => {
    res.change((x) => ({ ...x, ...shown }));
    try {
      const after = await f();
      res.setData(() => after);
      invalidate("observe/monitors");
    } catch (e) {
      toast({ title: `Couldn't ${what}`, description: errorMessage(e), tone: "failed" });
      invalidate(monitorRes(id).key);
    }
  });
  return (
    <>
      <title>{`${m.name} · Monitors · kiln`}</title>
      <header className="page-intro">
        <div className="min-w-0">
          <Crumbs index={8} items={[{ label: "Observe", to: "/observe" }, { label: "Monitors", to: "/observe/monitors" }, { label: m.id }]} />
          <h1>{m.name}</h1>
          <p className="lead flex flex-wrap items-center gap-3">
            <StateBadge state={m.state} muted={isMuted} />
            <code className="font-mono text-[12px] break-all text-[#a7aab3]">{m.composite?.named ?? m.query}</code>
            <HeldBy m={m} />
          </p>
        </div>
        {!editing && (
          <div className="flex flex-wrap items-center gap-2">
            <Button variant="secondary" onClick={() => setEditing(true)}>
              <Pencil /> Edit
            </Button>
            {isMuted ? (
              <Button variant="ghost" pending={muting} onClick={() => act("unmute it", { muted_until: null }, () => observeApi.unmute(m.id))}>
                <Bell /> Unmute
              </Button>
            ) : (
              <Button variant="ghost" pending={muting} onClick={() => act("mute it", { muted_until: Number.MAX_SAFE_INTEGER }, () => observeApi.mute(m.id))}>
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
          {m.composite ? (
            <Reads c={m.composite} />
          ) : (
            <section aria-label="What it watches">
              <div className="mb-4">
                <RangePicker value={range} onChange={setRange} />
              </div>
              <Loadable key={m.query} label="what it watches" fallback={<Hold height={246} />}>
                <Watched m={m} range={range} />
              </Loadable>
            </section>
          )}
          <Groups m={m} />
          <Panel eyebrow="Settings" title="How it is evaluated and what it says">
            <Facts
              items={[
                ...(m.composite
                  ? [{ label: "Alerts when", value: m.composite.expression }]
                  : isCheckQuery(m.query)
                    ? [
                        { label: "Alerts after", value: `${m.options.thresholds.critical ?? 1} CRITICAL in a row` },
                        { label: "Warns after", value: m.options.thresholds.warning ? `${m.options.thresholds.warning} WARNING in a row` : "—" },
                        { label: "Recovers after", value: `${m.options.thresholds.ok ?? 1} OK in a row` },
                      ]
                    : [
                        { label: "Critical", value: m.options.thresholds.critical ?? "the query's" },
                        { label: "Warning", value: m.options.thresholds.warning ?? "—" },
                        {
                          label: "Recovers below",
                          value: [m.options.thresholds.critical_recovery, m.options.thresholds.warning_recovery].filter((v) => v !== null && v !== undefined).join(" / ") || "its thresholds",
                        },
                      ]),
                {
                  label: "No data",
                  value: m.options.notify_no_data
                    ? m.options.no_data_timeframe
                      ? `after ${m.options.no_data_timeframe} min`
                      : isCheckQuery(m.query)
                        ? "after a day"
                        : "after two windows"
                    : "keeps its state",
                },
                ...(m.options.on_missing_data && m.options.on_missing_data !== "default"
                  ? [{ label: "A group without data", value: MISSING.find((x) => x.value === m.options.on_missing_data)?.label }]
                  : []),
                ...(m.options.timeout_h ? [{ label: "Resolves", value: `after ${m.options.timeout_h} h without data` }] : []),
                ...(m.options.group_retention_duration ? [{ label: "Forgets a group", value: `after ${m.options.group_retention_duration}` }] : []),
                { label: "Told again", value: toldAgain(m.options) },
                { label: "Evaluated", value: `every ${m.options.every ?? 60} s${m.options.evaluation_delay ? `, ${m.options.evaluation_delay} s late` : ""}` },
                ...(m.options.new_group_delay ? [{ label: "New groups wait", value: `${m.options.new_group_delay} s` }] : []),
                ...(m.options.require_full_window ? [{ label: "Its window", value: "full, or not evaluated" }] : []),
                ...(m.options.notify_by?.length ? [{ label: "Tells", value: m.options.notify_by.includes("*") ? "one notice for all" : `by ${m.options.notify_by.join(", ")}` }] : []),
                ...(noticeSaid(m.options) ? [{ label: "Its notice", value: noticeSaid(m.options) }] : []),
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
            await navigate("/observe/monitors");
          } catch (e) {
            toast({ title: "Couldn't delete the monitor", description: errorMessage(e), tone: "failed" });
          }
        }}
      />
    </>
  );
}

/** What a composite reads: each monitor, by its letter, with its state. */
function Reads({ c }: { c: CompositeOf }) {
  return (
    <section aria-label="What it reads">
      <h2 className="field-label">
        Alerts when <code className="font-mono normal-case">{c.expression}</code>
      </h2>
      <ul className="divide-y divide-[#1d1f23] border-y border-[#1d1f23]">
        {c.monitors.map((x) => (
          <li key={x.letter} className="flex items-center gap-3 px-2 py-2">
            <span className="w-4 shrink-0 font-mono text-[12px] text-[#8c8f98]">{x.letter}</span>
            <span className="min-w-0 flex-1 truncate text-[13px]">
              {x.name ? (
                <Link to={`/observe/monitors/${encodeURIComponent(x.id)}`} className="text-link">
                  {x.name}
                </Link>
              ) : (
                <span className="text-[#e2c4c6]">{x.id}: not there</span>
              )}
            </span>
            {x.state && <StateBadge state={x.state} />}
          </li>
        ))}
      </ul>
    </section>
  );
}

/** What the monitor watches over the range, its thresholds across it. */
function Watched({ m, range }: { m: Monitor; range: Range }) {
  const check = useRes(checkRes(m.query)).data;
  if (!check.ok) return <p className="empty-line py-10 text-center">{check.error}</p>;
  if (check.kind === "logs") return <LogsWatched search={check.watched?.search ?? ""} range={range} />;
  // A service check's groups, each one's latest run.
  if (check.kind === "service-check") return <CheckRuns ask={{ check: check.watched?.check ?? "", tags: check.watched?.over ?? [], group_by: check.watched?.by ?? [] }} range={range} height={220} />;
  if (check.kind === "spans") return <SpansWatched search={check.watched?.search ?? ""} range={range} />;
  // An Error Tracking monitor's issues' errors, by issue: drawn when its search says tags alone.
  if (check.kind === "error-tracking")
    return check.watched?.query ? (
      <QueryChart queries={[check.watched.query]} range={range} height={220} display="bars" />
    ) : (
      <p className="empty-line py-10 text-center">
        Its issues&apos; errors where <code className="font-mono">{check.watched?.search}</code> holds, by issue: its groups, below.
      </p>
    );
  const t = m.options.thresholds;
  // A change monitor's thresholds are of the change, not of the series drawn.
  const marks: Threshold[] =
    check.kind === "metric" || check.kind === "forecast"
      ? [{ label: "Critical", value: check.threshold ?? 0, color: SERIES.fail }, ...(t.warning !== null && t.warning !== undefined ? [{ label: "Warning", value: t.warning, color: SERIES.warn }] : [])]
      : [];
  // A forecast monitor's chart reaches as far ahead as it looks.
  return <QueryChart queries={[check.watched?.query ?? ""]} range={range} height={220} marks={marks} ahead={check.kind === "forecast" ? check.window_ms : undefined} />;
}

function LogsWatched({ search, range }: { search: string; range: Range }) {
  const h = useRes(histogramRes(logsOnly(search), range)).data;
  return <Histogram h={h} range={range} height={200} />;
}

function SpansWatched({ search, range }: { search: string; range: Range }) {
  const h = useRes(spanHistogramRes(search || "*", range)).data;
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
                  <td className="num cell-mono">{g.value === null ? "—" : isCheckQuery(m.query) ? CHECK_WORDS[CHECK_CODES[g.value] ?? "UNKNOWN"] : String(Number(g.value.toPrecision(4)))}</td>
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

/** A condition in letters (`a && !b`), each letter the monitor of its place; an error for a letter
 * that names none. */
function lettersToIds(cond: string, ids: string[]): { query: string } | { error: string } {
  let bad = "";
  const query = cond.replace(/\b[a-z]\b/g, (l) => {
    const id = ids[LETTERS.indexOf(l)];
    if (!id) bad ||= l;
    return id ?? l;
  });
  return bad ? { error: `${bad} names no monitor: choose one for it, or take it out` } : { query };
}

const num = (s: string): number | null => (s.trim() === "" || !Number.isFinite(Number(s)) ? null : Number(s));
const str = (v: number | null | undefined) => (v === null || v === undefined ? "" : String(v));

/** A monitor's query, thresholds, message and options; made or changed on save. */
function MonitorForm({ initial, onDone }: { initial: Monitor; onDone?: (after: Monitor | null) => void }) {
  const [name, setName] = React.useState(initial.name);
  const [kind, setKind] = React.useState(initial.type === COMPOSITE ? COMPOSITE : "query");
  const [query, setQuery] = React.useState(initial.type === COMPOSITE ? "" : initial.query);
  // A composite's monitors, by letter, and its condition in letters.
  const [parts, setParts] = React.useState<string[]>(initial.composite ? initial.composite.monitors.map((x) => x.id) : ["", ""]);
  const [cond, setCond] = React.useState(initial.composite?.expression ?? "a && b");
  const composite = kind === COMPOSITE;
  const [message, setMessage] = React.useState(initial.message);
  const [tags, setTags] = React.useState(initial.tags.join(", "));
  const [priority, setPriority] = React.useState(str(initial.priority));
  const t = initial.options.thresholds;
  const isCheck = !composite && isCheckQuery(query);
  // A service check's: runs in a row.
  const [crit, setCrit] = React.useState(str(t.critical));
  const [okRuns, setOkRuns] = React.useState(str(t.ok));
  const [warning, setWarning] = React.useState(str(t.warning));
  const [critRec, setCritRec] = React.useState(str(t.critical_recovery));
  const [warnRec, setWarnRec] = React.useState(str(t.warning_recovery));
  const [noData, setNoData] = React.useState(initial.options.notify_no_data);
  const [noDataMin, setNoDataMin] = React.useState(str(initial.options.no_data_timeframe));
  const [renotify, setRenotify] = React.useState(str(initial.options.renotify_interval));
  const [delay, setDelay] = React.useState(str(initial.options.evaluation_delay));
  const [every, setEvery] = React.useState(str(initial.options.every));
  const o = initial.options;
  const [missing, setMissing] = React.useState<string>(o.on_missing_data && o.on_missing_data !== "default" ? o.on_missing_data : "");
  const [timeoutH, setTimeoutH] = React.useState(str(o.timeout_h));
  const [retention, setRetention] = React.useState(o.group_retention_duration ?? "");
  const [statuses, setStatuses] = React.useState<RenotifyStatus[]>(o.renotify_statuses ?? RENOTIFY_DEFAULT);
  const [occurrences, setOccurrences] = React.useState(str(o.renotify_occurrences));
  const [escalation, setEscalation] = React.useState(o.escalation_message ?? "");
  const [newGroupDelay, setNewGroupDelay] = React.useState(str(o.new_group_delay));
  const [fullWindow, setFullWindow] = React.useState(o.require_full_window ?? false);
  const [notifyBy, setNotifyBy] = React.useState((o.notify_by ?? []).join(", "));
  const [includeTags, setIncludeTags] = React.useState(o.include_tags ?? true);
  const [preset, setPreset] = React.useState<string>(o.notification_preset_name && o.notification_preset_name !== "show_all" ? o.notification_preset_name : "");
  const [samples, setSamples] = React.useState(o.enable_logs_sample ?? false);
  const [audit, setAudit] = React.useState(o.notify_audit ?? false);
  const escalationId = React.useId();
  // Which of Datadog's options its kind takes: a monitor of records' (a formula's of logs, spans or
  // events too), a log monitor's, a metric monitor's.
  const records = !composite && (RECORDS.test(query) || (o.variables ?? []).some((v) => v.data_source !== "metrics"));
  const logs = !composite && /^\s*logs\(/.test(query);
  const metric = !composite && !isCheck && !records;
  const [checked, setCheck] = React.useState<{ of: string; c: MonitorCheck } | null>(null);
  const form = [
    name,
    kind,
    query,
    parts,
    cond,
    message,
    tags,
    priority,
    crit,
    okRuns,
    warning,
    critRec,
    warnRec,
    noData,
    noDataMin,
    renotify,
    delay,
    every,
    missing,
    timeoutH,
    retention,
    statuses,
    occurrences,
    escalation,
    newGroupDelay,
    fullWindow,
    notifyBy,
    includeTags,
    preset,
    samples,
    audit,
  ];
  const opening = useOpening(form);
  const navigate = useNavigate();

  const notifyByTags = notifyBy
    .split(",")
    .map((x) => x.trim())
    .filter(Boolean);
  /** What is sent as the query: a composite's condition with its monitors' ids. */
  const sent = (): { query: string } | { error: string } => (composite ? lettersToIds(cond.trim(), parts) : { query: query.trim() });
  const body = (q: string): MonitorIn => ({
    name: name.trim(),
    ...(composite ? { type: COMPOSITE } : isCheck ? { type: SERVICE_CHECK } : {}),
    query: q,
    message,
    tags: tags
      .split(",")
      .map((x) => x.trim())
      .filter(Boolean),
    priority: num(priority),
    options: {
      // What the form does not show (a formula's variables, a cumulative window's start, an anomaly
      // monitor's windows, a dynamic threshold) kept as it was.
      ...o,
      thresholds: isCheck
        ? { ...o.thresholds, critical: num(crit), warning: num(warning), ok: num(okRuns) }
        : { ...o.thresholds, critical: null, warning: num(warning), critical_recovery: num(critRec), warning_recovery: num(warnRec) },
      notify_no_data: noData && !(records && missing),
      no_data_timeframe: num(noDataMin),
      renotify_interval: num(renotify),
      renotify_statuses: renotify.trim() && !sameSet(statuses, RENOTIFY_DEFAULT) ? statuses : null,
      renotify_occurrences: num(occurrences),
      escalation_message: escalation.trim() ? escalation : null,
      evaluation_delay: num(delay),
      every: num(every),
      timeout_h: num(timeoutH),
      require_full_window: metric && fullWindow,
      new_group_delay: num(newGroupDelay),
      include_tags: includeTags ? null : false,
      on_missing_data: records && missing ? (missing as OnMissingData) : null,
      notify_audit: audit,
      notify_by: notifyByTags.length ? notifyByTags : null,
      group_retention_duration: records && retention.trim() ? retention.trim() : null,
      notification_preset_name: preset ? (preset as NotificationPreset) : null,
      enable_logs_sample: logs && samples,
    },
  });
  // What a check is of. One shows only under it: a check that comes after an edit was of what was
  // there before (W21-R15).
  const asked = () => JSON.stringify([kind, sent()]);
  const check = checked?.of === asked() ? checked.c : null;
  const [save, saving] = useAction(async () => {
    const s = sent();
    const of = JSON.stringify([kind, s]);
    if ("error" in s) {
      setCheck({ of, c: { ok: false, error: s.error } });
      return;
    }
    const c = await observeApi.validateMonitor(s.query, undefined, composite ? COMPOSITE : undefined);
    setCheck({ of, c });
    if (!c.ok) return;
    const after = initial.id ? await observeApi.updateMonitor(initial.id, body(s.query)) : await observeApi.createMonitor(body(s.query));
    invalidate("observe/monitors");
    // Done with only if it's still this form, saying what was saved: changed since, it stays, and the
    // page behind it shows what was (W21-R15).
    if (!opening.still(form)) {
      if (initial.id) invalidate(monitorRes(initial.id).key);
      return;
    }
    if (onDone) onDone(after);
    else await navigate(`/observe/monitors/${encodeURIComponent(after.id)}`);
  }, "Couldn't save the monitor");
  const [checkQuery, checking] = useAction(async () => {
    const of = asked();
    setCheck({ of, c: await observeApi.validateMonitor(query.trim()) });
  }, "Couldn't check the query");

  return (
    <form
      ref={opening.ref}
      className="grid grid-cols-1 gap-8 lg:grid-cols-[minmax(0,1fr)_300px]"
      onSubmit={(e) => {
        e.preventDefault();
        save();
      }}
    >
      <HeldFields held={saving && !initial.id} className="space-y-5">
        <TextField label="Name" value={name} onChange={(e) => setName(e.target.value)} placeholder="Disk filling on {{host.name}}" required />
        {!initial.id && (
          <Segmented
            label="Kind"
            value={kind}
            onChange={(v) => {
              setKind(v);
              setCheck(null);
              // The default message follows the kind; one written stays.
              if (message === MESSAGE.query || message === MESSAGE.composite) setMessage(v === COMPOSITE ? MESSAGE.composite : MESSAGE.query);
            }}
            options={[
              { value: "query", label: "Query" },
              { value: COMPOSITE, label: "Composite" },
            ]}
          />
        )}
        {composite ? (
          <Loadable label="the monitors" fallback={<RowsHold rows={3} rowHeight={40} />}>
            <CompositeFields self={initial.id} parts={parts} setParts={setParts} cond={cond} setCond={setCond} check={check} clearCheck={() => setCheck(null)} />
          </Loadable>
        ) : (
          <>
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
                    // The default message follows what it watches; one written stays.
                    const next = isCheckQuery(e.target.value) ? MESSAGE.check : MESSAGE.query;
                    if (message === MESSAGE.query || message === MESSAGE.check) setMessage(next);
                  }}
                  placeholder="avg(last_5m):max:system.disk.in_use{*} by {host,mount} > 0.9"
                  spellCheck={false}
                  required
                />
              </div>
              <div className="field-hint flex flex-wrap items-center gap-3">
                {check ? (
                  check.ok && check.kind === "service-check" ? (
                    <span>
                      Moves each group{check.watched?.by?.length ? ` (by ${check.watched.by.join(", ")})` : ""} of <code className="font-mono">{check.watched?.check}</code> by its latest runs.
                    </span>
                  ) : check.ok ? (
                    <span>
                      Alerts when{" "}
                      {check.kind === "logs" || check.kind === "spans"
                        ? "the count"
                        : check.kind === "change"
                          ? "the change"
                          : check.kind === "forecast"
                            ? "the forecast's edge"
                            : check.kind === "error-tracking"
                              ? "an issue's error count"
                              : "the value"}{" "}
                      is {check.op} {check.threshold}.
                    </span>
                  ) : (
                    <span className="text-[#e2c4c6]">{check.error}</span>
                  )
                ) : (
                  <span>Datadog&apos;s monitor syntax: a metric, its change, logs counted, or a service check&apos;s runs.</span>
                )}
                <button type="button" className="text-link" {...sendingProps(checking)} onClick={checkQuery}>
                  Check
                </button>
              </div>
            </div>
            {isCheck ? (
              <div className="grid gap-3 sm:grid-cols-3">
                <TextField label="Alerts after CRITICAL runs in a row" inputMode="numeric" value={crit} onChange={(e) => setCrit(e.target.value)} placeholder="1" />
                <TextField label="Warns after WARNING runs in a row" inputMode="numeric" value={warning} onChange={(e) => setWarning(e.target.value)} placeholder="never" />
                <TextField label="Recovers after OK runs in a row" inputMode="numeric" value={okRuns} onChange={(e) => setOkRuns(e.target.value)} placeholder="1" />
              </div>
            ) : (
              <div className="grid gap-3 sm:grid-cols-3">
                <TextField label="Warning at" inputMode="decimal" value={warning} onChange={(e) => setWarning(e.target.value)} />
                <TextField label="Recovers from critical at" inputMode="decimal" value={critRec} onChange={(e) => setCritRec(e.target.value)} />
                <TextField label="Recovers from warning at" inputMode="decimal" value={warnRec} onChange={(e) => setWarnRec(e.target.value)} />
              </div>
            )}
          </>
        )}
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
      </HeldFields>
      <aside className="space-y-4">
        <HeldFields held={saving && !initial.id}>
          <label className="flex items-center gap-2 text-[13px] text-[#c8cad0]">
            <input type="checkbox" className="check" checked={noData && !(records && missing)} disabled={records && Boolean(missing)} onChange={(e) => setNoData(e.target.checked)} /> Say when data
            stops
          </label>
          {noData && !(records && missing) && (
            <TextField label="After minutes without data" inputMode="numeric" value={noDataMin} onChange={(e) => setNoDataMin(e.target.value)} placeholder="two windows" />
          )}
          <TextField label="Tell again every (minutes)" inputMode="numeric" value={renotify} onChange={(e) => setRenotify(e.target.value)} placeholder="once" />
          <TextField label="Evaluate every (seconds)" inputMode="numeric" value={every} onChange={(e) => setEvery(e.target.value)} placeholder="60" />
          <TextField label="Wait for late data (seconds)" inputMode="numeric" value={delay} onChange={(e) => setDelay(e.target.value)} placeholder="0" />
          <TextField label="Tags" value={tags} onChange={(e) => setTags(e.target.value)} placeholder="team:ci, service:api" />
          <SelectField label="Priority" value={priority} onChange={setPriority} options={[{ value: "", label: "None" }, ...[1, 2, 3, 4, 5].map((p) => ({ value: String(p), label: `P${p}` }))]} />
          <Fold title="Missing data" set={[records && missing, timeoutH, records && retention.trim()].filter(Boolean).length}>
            {records && (
              <SelectField
                label="A group without data"
                value={missing}
                onChange={(v) => {
                  setMissing(v);
                  if (v) setNoData(false);
                }}
                options={MISSING}
              />
            )}
            <SelectField
              label="Resolves after, without data"
              value={timeoutH}
              onChange={setTimeoutH}
              options={[{ value: "", label: "Never" }, ...TIMEOUT_HOURS.map((h) => ({ value: String(h), label: h === 1 ? "1 hour" : `${h} hours` }))]}
            />
            {records && <TextField label="Forgets a group after" value={retention} onChange={(e) => setRetention(e.target.value)} placeholder="a day" hint="60m to 72h: 1h, 2d" />}
          </Fold>
          <Fold title="Telling again" set={[!sameSet(statuses, RENOTIFY_DEFAULT), occurrences.trim(), escalation.trim()].filter(Boolean).length}>
            <fieldset>
              <legend className="field-label">Told again in</legend>
              <div className="flex flex-wrap gap-x-4 gap-y-2">
                {RENOTIFY.map((r) => (
                  <label key={r.value} className="flex items-center gap-2">
                    <input
                      type="checkbox"
                      className="check"
                      checked={statuses.includes(r.value)}
                      onChange={(e) => setStatuses(e.target.checked ? [...statuses, r.value] : statuses.filter((x) => x !== r.value))}
                    />
                    <StateBadge state={r.state} />
                  </label>
                ))}
              </div>
              {!renotify.trim() && <p className="field-hint">Once it is told again every so many minutes.</p>}
            </fieldset>
            <TextField label="At most (times)" inputMode="numeric" value={occurrences} onChange={(e) => setOccurrences(e.target.value)} placeholder="no end" />
            <div>
              <label htmlFor={escalationId} className="field-label">
                Escalation message
              </label>
              <div className="field">
                <textarea id={escalationId} rows={3} className="font-mono" value={escalation} onChange={(e) => setEscalation(e.target.value)} placeholder="Said after the message, told again" />
              </div>
            </div>
          </Fold>
          <Fold title="Groups" set={[newGroupDelay.trim(), metric && fullWindow, notifyBy.trim()].filter(Boolean).length}>
            <TextField label="New groups wait (seconds)" inputMode="numeric" value={newGroupDelay} onChange={(e) => setNewGroupDelay(e.target.value)} placeholder="0" />
            {metric && (
              <label className="flex items-center gap-2 text-[13px] text-[#c8cad0]">
                <input type="checkbox" className="check" checked={fullWindow} onChange={(e) => setFullWindow(e.target.checked)} /> Only a full window
              </label>
            )}
            <TextField label="Tell by" value={notifyBy} onChange={(e) => setNotifyBy(e.target.value)} placeholder="each group" hint="Some of its group tags; * for one notice" />
          </Fold>
          <Fold title="The notice" set={[!includeTags, preset, logs && samples, audit].filter(Boolean).length}>
            <label className="flex items-center gap-2 text-[13px] text-[#c8cad0]">
              <input type="checkbox" className="check" checked={includeTags} onChange={(e) => setIncludeTags(e.target.checked)} /> Its group in the title
            </label>
            <SelectField label="Shows besides its message" value={preset} onChange={setPreset} options={PRESETS} />
            {logs && (
              <label className="flex items-center gap-2 text-[13px] text-[#c8cad0]">
                <input type="checkbox" className="check" checked={samples} onChange={(e) => setSamples(e.target.checked)} /> Its group&apos;s latest logs
              </label>
            )}
            <label className="flex items-center gap-2 text-[13px] text-[#c8cad0]">
              <input type="checkbox" className="check" checked={audit} onChange={(e) => setAudit(e.target.checked)} /> Its changes told to its mentions
            </label>
          </Fold>
        </HeldFields>
        <div className="flex gap-2 pt-2">
          <Button type="submit" disabled={!name.trim() || (composite ? !cond.trim() : !query.trim())} pending={saving}>
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

/** Some of a form's settings, folded in place under their name: open as the form opens while any of
 * them is set (how many, beside its name), and as you leave it. */
function Fold({ title, set, children }: { title: string; set: number; children: React.ReactNode }) {
  const [open, setOpen] = React.useState(set > 0);
  const body = React.useId();
  return (
    <div className="border-t border-[#20232b] pt-3">
      <div className="section-fold" data-open={open || undefined}>
        <button type="button" className="section-toggle font-mono text-[9px] tracking-[0.1em] text-[#8f939e] uppercase" aria-expanded={open} aria-controls={body} onClick={() => setOpen(!open)}>
          {title}
          {set > 0 && <span className="tracking-normal text-[#c8cad0] normal-case">{set} set</span>}
          <ChevronDown size={14} className="section-chev" aria-hidden="true" />
        </button>
      </div>
      <div id={body} className={open ? "mt-3 space-y-4" : undefined}>
        {open && children}
      </div>
    </div>
  );
}

/** A composite's monitors, lettered as Datadog's form letters them, and its condition over the
 * letters: ten at the most, none a composite (nor this one). */
function CompositeFields({
  self,
  parts,
  setParts,
  cond,
  setCond,
  check,
  clearCheck,
}: {
  self: string;
  parts: string[];
  setParts: (p: string[]) => void;
  cond: string;
  setCond: (c: string) => void;
  check: MonitorCheck | null;
  clearCheck: () => void;
}) {
  const all = useRes(monitorsRes).data.filter((m) => m.type !== COMPOSITE && m.id !== self);
  const options = [{ value: "", label: "Choose a monitor" }, ...all.map((m) => ({ value: m.id, label: m.name }))];
  const set = (i: number, id: string) => {
    setParts(parts.map((p, j) => (j === i ? id : p)));
    clearCheck();
  };
  return (
    <div className="space-y-3">
      <ul className="space-y-2" aria-label="Its monitors">
        {parts.map((p, i) => (
          <li key={i} className="flex items-end gap-3">
            <span className="w-4 shrink-0 pb-2.5 font-mono text-[13px] text-[#8c8f98]">{LETTERS[i]}</span>
            <SelectField label={`Monitor ${LETTERS[i]}`} hideLabel className="min-w-0 flex-1" value={p} onChange={(v) => set(i, v)} options={options} />
            {parts.length > 1 && (
              <Button
                variant="ghost"
                size="icon"
                aria-label={`Take ${LETTERS[i]} out`}
                onClick={() => {
                  setParts(parts.filter((_, j) => j !== i));
                  clearCheck();
                }}
              >
                <Trash2 />
              </Button>
            )}
          </li>
        ))}
      </ul>
      {parts.length < LETTERS.length && (
        <button type="button" className="text-link text-[12.5px]" onClick={() => setParts([...parts, ""])}>
          Add a monitor
        </button>
      )}
      <div>
        <TextField
          label="Alerts when"
          className="font-mono"
          value={cond}
          onChange={(e) => {
            setCond(e.target.value);
            clearCheck();
          }}
          placeholder="a && b"
          spellCheck={false}
          autoComplete="off"
        />
        <p className="field-hint">
          {check && !check.ok ? (
            <span className="text-[#e2c4c6]">{check.error}</span>
          ) : (
            <>
              <code className="font-mono">&amp;&amp;</code>, <code className="font-mono">||</code>, <code className="font-mono">!</code> and parentheses over the letters. Each group alerts as severely
              as the condition says: an <code className="font-mono">&amp;&amp;</code> as the least of its two, an <code className="font-mono">||</code> as the most.
            </>
          )}
        </p>
      </div>
    </div>
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

/** A downtime that holds the monitor now, or the next that will: a link to the downtimes. */
function HeldBy({ m }: { m: Monitor }) {
  const d = m.downtimes?.find((x) => x.status === "active") ?? m.downtimes?.find((x) => x.status === "scheduled");
  if (!d?.window) return null;
  const at = (ms: number) => new Date(ms).toLocaleString(undefined, { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" });
  const text = d.status === "active" ? `Held by a downtime${d.window.end ? ` until ${at(d.window.end)}` : ""}` : `A downtime from ${at(d.window.start)}`;
  return (
    <Link to="/observe/downtimes" className="text-[12.5px] text-[#a7aab3] hover:text-white">
      {text}
    </Link>
  );
}
