/**
 * One object of an app, in the page: what it is and how it stands (kind, namespace, age, ready
 * counts, images, owners, conditions, labels), its live and desired manifests, what a sync would
 * change in it, its events, and — for pods and what runs them — its logs, read once or followed
 * as they are written. A select chooses which; nothing opens over the page.
 */
import * as React from "react";
import { X } from "lucide-react";

import { cdApi, resourceKey, type CdLogQuery } from "@/api/cd";
import { wsUrl } from "@/api/client";
import type { CdLogLine, CdLogs, CdNode, CdResourceDetail, CdResourceKey } from "@/api/types";
import { Button } from "@/components/ui/button";
import { RowsHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { CopyButton, SelectField } from "@/components/ui/misc";
import { useNow } from "@/lib/hooks";
import { useSuspenseResource } from "@/lib/resource";
import { FileDiffView } from "@/views/harness/diff";
import { age, compareText } from "./model";
import { HealthPair, KindBadge, StatePair, SyncPair, appKey, objectLabel } from "./parts";
import { Highlighted } from "@/components/code/highlighted";

export type Show = "summary" | "live" | "desired" | "diff" | "events" | "logs";

/** Kinds that run pods, whose logs are theirs. */
const RUNS_PODS = new Set(["Pod", "Deployment", "ReplicaSet", "StatefulSet", "DaemonSet", "Job", "CronJob"]);

/** It has logs to read: it runs pods, and is in the cluster. */
export const hasLogs = (n: CdNode) => RUNS_PODS.has(n.kind) && !n.missing;

const when = (t: string | null | undefined) => {
  if (!t) return "";
  const d = Date.parse(t);
  return Number.isNaN(d) ? t : new Date(d).toLocaleString();
};

/** Brought into view as it opens, gently unless motion is reduced. */
function intoView(el: HTMLElement | null) {
  if (!el) return;
  const still = window.matchMedia("(prefers-reduced-motion: reduce)").matches || document.documentElement.dataset.motion === "paused";
  el.scrollIntoView({ block: "nearest", behavior: still ? "auto" : "smooth" });
}

const keyOf = (n: CdNode): CdResourceKey => ({ group: n.group, kind: n.kind, namespace: n.namespace, name: n.name });

export function ResourceDetails({
  appId,
  node,
  onClose,
  onSelect,
  initial,
}: {
  appId: string;
  node: CdNode;
  onClose: () => void;
  /** Go to another node (an owner). */
  onSelect: (id: string) => void;
  /** What it opens on (its logs, its events), when asked for. */
  initial?: Show;
}) {
  const now = useNow();
  const logs = hasLogs(node);
  const options: { value: Show; label: string }[] = [
    { value: "summary", label: "Summary" },
    { value: "live", label: "Live manifest" },
    ...(node.managed ? [{ value: "desired" as const, label: "Desired manifest" }, { value: "diff" as const, label: "Diff" }] : []),
    { value: "events", label: "Events" },
    ...(logs ? [{ value: "logs" as const, label: "Logs" }] : []),
  ];
  const [show, setShow] = React.useState<Show>(initial ?? (node.kind === "Pod" && (node.health === "Degraded" || (node.restarts ?? 0) > 0) ? "logs" : "summary"));
  const current = options.some((o) => o.value === show) ? show : "summary";

  return (
    <section ref={intoView} className="cd-details" aria-label={`${node.kind} ${node.name}`}>
      <header className="cd-details-head">
        <div className="cd-details-title">
          <KindBadge kind={node.kind} group={node.group} />
          <h2>{node.name}</h2>
        </div>
        <button type="button" className="cd-close" onClick={onClose} aria-label="Close the details">
          <X size={15} />
        </button>
      </header>
      <p className="cd-details-meta">
        {[node.kind, node.namespace || "cluster-scoped", node.created_at ? `${age(node.created_at, now)} old` : "", node.node ? `on ${node.node}` : ""].filter(Boolean).join(" · ")}
      </p>
      <div className="cd-details-state">
        <HealthPair health={node.health} />
        {node.managed && !node.hook && node.sync && <SyncPair sync={node.sync} />}
        {node.managed ? <span className="cd-meta">managed</span> : <span className="cd-meta">live, owned by the app's objects</span>}
      </div>
      {(node.message || node.why) && (
        <p className="cd-details-why" data-tone={node.why ? "warn" : node.health === "Degraded" || node.health === "Missing" ? "fail" : node.health === "Healthy" ? undefined : "warn"}>
          {node.why ?? node.message}
        </p>
      )}
      {/* What it shows changes once that is in: until then, what was there stays. */}
      <SelectField label="Show" value={current} onChange={(v) => React.startTransition(() => setShow(v as Show))} options={options} className="cd-details-show" />
      <div className="cd-details-body">
        <Loadable label="the object" fallback={<RowsHold rows={4} rowHeight={40} />}>
          {current === "logs" ? (
            <Logs appId={appId} node={node} />
          ) : current === "events" ? (
            <Events appId={appId} node={node} />
          ) : (
            <Detail appId={appId} node={node} show={current} onSelect={onSelect} />
          )}
        </Loadable>
      </div>
    </section>
  );
}

function Detail({ appId, node, show, onSelect }: { appId: string; node: CdNode; show: Show; onSelect: (id: string) => void }) {
  const k = keyOf(node);
  const d = useSuspenseResource(`${appKey(appId)}object/${resourceKey(k)}`, (s) => cdApi.resource(appId, k, s)).data;
  if (show === "summary") return <Summary d={d} node={node} onSelect={onSelect} />;
  if (show === "live") return <Manifest text={d.live} empty="It isn't in the cluster." name={`${node.name}.live.yaml`} />;
  if (show === "desired") return <Manifest text={d.desired} empty="Git doesn't hold it any more." name={`${node.name}.yaml`} />;
  return <Diff d={d} />;
}

function Fact({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div>
      <dt>{label}</dt>
      <dd>{children}</dd>
    </div>
  );
}

function Summary({ d, node, onSelect }: { d: CdResourceDetail; node: CdNode; onSelect: (id: string) => void }) {
  const labels = Object.entries(d.labels).sort(([a], [b]) => compareText(a, b));
  const annotations = Object.entries(d.annotations).sort(([a], [b]) => compareText(a, b));
  const main = node.containers.filter((c) => !c.init);
  const init = node.containers.filter((c) => c.init);
  return (
    <div className="cd-summary">
      {d.errors.length > 0 && <p className="cd-warn">{d.errors.join("; ")}</p>}
      <dl className="cd-facts">
        <Fact label="Kind">
          {node.kind} <span className="cd-dim">{node.group ? `${node.group}/${node.version}` : node.version}</span>
        </Fact>
        <Fact label="Namespace">{node.namespace || "Cluster-scoped"}</Fact>
        <Fact label="Created">{node.created_at ? when(node.created_at) : "Not in the cluster"}</Fact>
        {node.status && <Fact label="Status">{node.status}</Fact>}
        {node.ready && <Fact label={node.kind === "Job" ? "Completions" : "Ready"}>{node.ready}</Fact>}
        {node.restarts !== null && <Fact label="Restarts">{node.restarts}</Fact>}
        {node.node && <Fact label="Node">{node.node}</Fact>}
        {node.info
          .filter((i) => !["Status", "Ready", "Restarts", "Node", "Completions"].includes(i.name))
          .map((i) => (
            <Fact key={i.name} label={i.name}>
              {i.value}
            </Fact>
          ))}
      </dl>

      {d.owners.length > 0 && (
        <section className="cd-sub">
          <h3>Owned by</h3>
          <ol className="cd-owners">
            {d.owners.map((o) => (
              <li key={o.id}>
                <button type="button" className="cd-link" onClick={() => onSelect(o.id)}>
                  {objectLabel(o)}
                </button>
              </li>
            ))}
          </ol>
        </section>
      )}

      {main.length + init.length > 0 && (
        <section className="cd-sub">
          <h3>{node.kind === "Pod" ? "Containers" : "Pod template"}</h3>
          <ul className="cd-containers">
            {[...init, ...main].map((c) => (
              <li key={`${c.init}-${c.name}`}>
                <span className="cd-c-name">
                  {c.name}
                  {c.init ? <span className="cd-dim"> · init</span> : null}
                </span>
                <span className="cd-c-image">{c.image}</span>
                {c.state && (
                  <span className="cd-c-state" data-bad={c.state === "waiting" || (c.state === "terminated" && c.reason !== "Completed") || undefined}>
                    {[c.reason ?? c.state, c.ready ? "ready" : c.state === "running" ? "not ready" : "", c.restarts ? `${c.restarts} restarts` : "", c.last ? `last: ${c.last}` : ""]
                      .filter(Boolean)
                      .join(" · ")}
                  </span>
                )}
              </li>
            ))}
          </ul>
        </section>
      )}

      {d.conditions.length > 0 && (
        <section className="cd-sub">
          <h3>Conditions</h3>
          <ul className="cd-conditions">
            {d.conditions.map((c) => (
              <li key={c.type} data-false={c.status !== "True" || undefined}>
                <span className="cd-c-name">{c.type}</span>
                <span className="cd-mono">{c.status}</span>
                <span className="cd-c-msg">{[c.reason, c.message].filter(Boolean).join(": ")}</span>
                {c.last && <span className="cd-dim">{when(c.last)}</span>}
              </li>
            ))}
          </ul>
        </section>
      )}

      <section className="cd-sub">
        <h3>Labels</h3>
        {labels.length ? (
          <ul className="cd-kv">
            {labels.map(([k, v]) => (
              <li key={k}>
                <span className="cd-k">{k}</span>
                <span className="cd-v">{v}</span>
              </li>
            ))}
          </ul>
        ) : (
          <p className="cd-dim">None.</p>
        )}
      </section>
      {annotations.length > 0 && (
        <section className="cd-sub">
          <h3>Annotations</h3>
          <ul className="cd-kv">
            {annotations.map(([k, v]) => (
              <li key={k}>
                <span className="cd-k">{k}</span>
                <span className="cd-v">{v}</span>
              </li>
            ))}
          </ul>
        </section>
      )}
      {d.finalizers.length > 0 && (
        <section className="cd-sub">
          <h3>Finalizers</h3>
          <p className="cd-mono">{d.finalizers.join(", ")}</p>
        </section>
      )}
    </div>
  );
}

function Manifest({ text, empty, name }: { text: string | null; empty: string; name: string }) {
  if (!text) return <p className="cd-empty">{empty}</p>;
  return (
    <div className="cd-manifest">
      <div className="cd-manifest-bar">
        <span className="cd-meta">{name}</span>
        <CopyButton text={text} className="cd-copy" />
      </div>
      <pre className="cd-yaml">
        <Highlighted text={text} lang="yaml" />
      </pre>
    </div>
  );
}

function Diff({ d }: { d: CdResourceDetail }) {
  if (!d.diff) return <p className="cd-empty">kiln doesn't manage it, so there is nothing to compare.</p>;
  if (d.diff.hunks.length === 0) return <p className="cd-empty">It matches git: a sync changes nothing in it.</p>;
  return (
    <div className="cd-diffs">
      <p className="cd-meta">What a sync changes: − is what runs, + what it becomes. Fields the cluster fills in itself are left out.</p>
      <FileDiffView diff={{ path: objectLabel(d.key), hunks: d.diff.hunks }} label={objectLabel(d.key)} language="yaml" />
    </div>
  );
}

function Events({ appId, node }: { appId: string; node: CdNode }) {
  const now = useNow();
  const k = keyOf(node);
  const list = useSuspenseResource(`${appKey(appId)}events/${resourceKey(k)}`, (s) => cdApi.events(appId, k, s), { pollMs: 15_000 }).data.events;
  if (list.length === 0) return <p className="cd-empty">No events. Kubernetes keeps them for about an hour.</p>;
  return (
    <ul className="cd-events" aria-label="Events">
      {list.map((e, i) => (
        <li key={`${e.last}-${e.reason}-${i}`} className="cd-event" data-warning={e.type === "Warning" || undefined}>
          <span className="cd-event-top">
            <StatePair tone={e.type === "Warning" ? "timed_out" : "skipped"} word={e.type} />
            <span className="cd-event-reason">{e.reason}</span>
            {e.count > 1 && <span className="cd-dim">×{e.count}</span>}
            <span className="cd-event-when" title={when(e.last)}>
              {age(e.last, now)} ago
            </span>
          </span>
          <span className="cd-event-msg">{e.message}</span>
          {e.source && <span className="cd-dim">{e.source}</span>}
        </li>
      ))}
    </ul>
  );
}

const SINCE = [
  { value: "", label: "Whole log" },
  { value: "300", label: "Last 5 minutes" },
  { value: "3600", label: "Last hour" },
  { value: "21600", label: "Last 6 hours" },
  { value: "86400", label: "Last day" },
];
const TAIL = [
  { value: "100", label: "Last 100 lines" },
  { value: "500", label: "Last 500 lines" },
  { value: "2000", label: "Last 2,000 lines" },
  { value: "10000", label: "Last 10,000 lines" },
];
/** Lines a followed log keeps. */
const KEEP = 10_000;

/** How a log is read: its pod and container, how far back, and how it shows. */
interface LogOpts {
  container: string;
  pod: string;
  since: string;
  tail: string;
  follow: boolean;
  wrap: boolean;
  previous: boolean;
  times: boolean;
}

function Logs({ appId, node }: { appId: string; node: CdNode }) {
  const [o, setO] = React.useState<LogOpts>({ container: "", pod: "", since: "", tail: "500", follow: false, wrap: true, previous: false, times: false });
  // A new way of reading it shows once read: until then, the log there was stays.
  const set = (patch: Partial<LogOpts>) => React.startTransition(() => setO((cur) => ({ ...cur, ...patch })));
  const q: CdLogQuery = {
    container: o.container || undefined,
    pod: o.pod || undefined,
    since_seconds: o.since ? Number(o.since) : undefined,
    tail_lines: Number(o.tail),
    previous: o.previous || undefined,
  };
  return o.follow ? <FollowedLog appId={appId} node={node} q={q} o={o} set={set} /> : <ReadLog appId={appId} node={node} q={q} o={o} set={set} />;
}

/** The log read once (and again when asked). */
function useReadLog(appId: string, node: CdNode, q: CdLogQuery) {
  const k = keyOf(node);
  return useSuspenseResource(`${appKey(appId)}logs/${resourceKey(k)}?${JSON.stringify(q)}`, (s) => cdApi.logs(appId, k, q, s));
}

function ReadLog({ appId, node, q, o, set }: { appId: string; node: CdNode; q: CdLogQuery; o: LogOpts; set: (p: Partial<LogOpts>) => void }) {
  const read = useReadLog(appId, node, q);
  const r = read.data;
  return <LogView node={node} o={o} set={set} known={r} lines={r.lines} errors={r.errors} state={read.refreshing ? "Reading…" : `${r.lines.length.toLocaleString()} lines`} again={read.reload} />;
}

type FollowState = "loading" | "live" | "ended";

/** The log followed: a WebSocket says each line as it is written (opened with its view, closed with it). */
function FollowedLog({ appId, node, q, o, set }: { appId: string; node: CdNode; q: CdLogQuery; o: LogOpts; set: (p: Partial<LogOpts>) => void }) {
  // The pods and containers there are, as the last read found them.
  const known = useReadLog(appId, node, { ...q, previous: undefined }).data;
  const [lines, setLines] = React.useState<CdLogLine[]>([]);
  const [errors, setErrors] = React.useState<string[]>([]);
  const [state, setState] = React.useState<FollowState>("loading");
  const k = keyOf(node);
  const path = cdApi.logsFollowPath(appId, k, q);
  const stream = React.useCallback(
    (el: HTMLDivElement | null) => {
      if (!el) return;
      setLines([]);
      setErrors([]);
      setState("loading");
      const ws = new WebSocket(wsUrl(path));
      let pending: CdLogLine[] = [];
      let frame = 0;
      const flush = () => {
        frame = 0;
        const add = pending;
        pending = [];
        setLines((l) => {
          const next = l.concat(add);
          return next.length > KEEP ? next.slice(next.length - KEEP) : next;
        });
      };
      ws.onopen = () => setState("live");
      ws.onmessage = (m) => {
        if (typeof m.data !== "string") return;
        const msg = JSON.parse(m.data) as { type: string; lines?: CdLogLine[]; pod?: string; message?: string };
        if (msg.type === "lines" && msg.lines) {
          pending.push(...msg.lines);
          if (!frame) frame = requestAnimationFrame(flush);
        } else if (msg.type === "error") {
          setErrors((e) => [...e, msg.pod ? `${msg.pod}: ${msg.message ?? ""}` : (msg.message ?? "")]);
        } else if (msg.type === "end") {
          setState("ended");
        }
      };
      ws.onclose = () => setState((s) => (s === "live" || s === "loading" ? "ended" : s));
      ws.onerror = () => setErrors((e) => [...e, "The log stream failed."]);
      return () => {
        if (frame) cancelAnimationFrame(frame);
        ws.close();
      };
    },
    [path],
  );
  return (
    <div ref={stream}>
      <LogView node={node} o={o} set={set} known={known} lines={lines} errors={errors} state={state === "live" ? "Following" : state === "ended" ? "Ended" : "Reading…"} />
    </div>
  );
}

function LogView({
  node,
  o,
  set,
  known,
  lines,
  errors,
  state,
  again,
}: {
  node: CdNode;
  o: LogOpts;
  set: (p: Partial<LogOpts>) => void;
  /** What the last read found: its pods and containers. */
  known: CdLogs;
  lines: CdLogLine[];
  errors: string[];
  state: string;
  /** Read it again (read once only). */
  again?: () => void;
}) {
  const stick = React.useRef(true);
  const pods = known.pods;
  const more = known.more_pods;
  const containers = known.containers.length ? known.containers : node.containers.filter((c) => !c.init).map((c) => c.name);
  const many = pods.length > 1 && !o.pod;
  const stamp = (ts: string) => ts.replace("T", " ").replace(/\.\d+Z$/, "Z");
  const text = React.useMemo(() => lines.map((l) => [o.times && l.ts ? stamp(l.ts) : "", many ? l.pod : "", l.text].filter(Boolean).join("  ")).join("\n"), [lines, o.times, many]);
  // Kept at the bottom as it grows, unless you scrolled up to read.
  const bottom = React.useCallback(
    (el: HTMLPreElement | null) => {
      if (el && stick.current) el.scrollTop = el.scrollHeight;
    },
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [text],
  );
  const podOptions = [{ value: "", label: pods.length > 1 || more ? `All ${pods.length + more} pods` : "Its pod" }, ...pods.map((p) => ({ value: p, label: p }))];
  return (
    <div className="cd-logs">
      <div className="cd-logs-bar">
        {containers.length > 1 && (
          <SelectField
            label="Container"
            value={o.container || containers[0] || ""}
            onChange={(container) => set({ container })}
            options={containers.map((c) => ({ value: c, label: c }))}
            className="cd-logs-select"
          />
        )}
        {node.kind !== "Pod" && pods.length > 1 && <SelectField label="Pod" value={o.pod} onChange={(pod) => set({ pod })} options={podOptions} className="cd-logs-select" />}
        <SelectField label="Since" value={o.since} onChange={(since) => set({ since })} options={SINCE} className="cd-logs-select" />
        <SelectField label="Lines" value={o.tail} onChange={(tail) => set({ tail })} options={TAIL} className="cd-logs-select" />
      </div>
      <div className="cd-logs-bar">
        <label className="cd-opt">
          <input type="checkbox" className="check" checked={o.follow} onChange={(e) => set({ follow: e.target.checked })} /> Follow
        </label>
        <label className="cd-opt cd-opt-wrap">
          <input type="checkbox" className="check" checked={o.wrap} onChange={(e) => set({ wrap: e.target.checked })} /> Wrap
        </label>
        <label className="cd-opt">
          <input type="checkbox" className="check" checked={o.times} onChange={(e) => set({ times: e.target.checked })} /> Times
        </label>
        {!o.follow && (
          <label className="cd-opt" title="The run before the container last restarted">
            <input type="checkbox" className="check" checked={o.previous} onChange={(e) => set({ previous: e.target.checked })} /> Previous run
          </label>
        )}
        <span className="cd-logs-end">
          <span className="cd-meta" aria-live="polite">
            {state}
          </span>
          {again && (
            <Button size="xs" variant="ghost" onClick={again}>
              Read again
            </Button>
          )}
          <CopyButton text={text} className="cd-copy" />
        </span>
      </div>
      {errors.length > 0 && <p className="cd-warn">{errors.join("; ")}</p>}
      {more > 0 && (
        <p className="cd-meta">
          Showing {pods.length} of {pods.length + more} pods: the newest.
        </p>
      )}
      <pre
        ref={bottom}
        className="cd-log"
        data-wrap={o.wrap || undefined}
        tabIndex={0}
        aria-label="Log"
        onScroll={(e) => {
          const el = e.currentTarget;
          stick.current = el.scrollHeight - el.scrollTop - el.clientHeight < 24;
        }}
      >
        {lines.length === 0 ? <span className="cd-dim">{state === "Reading…" ? "Reading the log…" : "Nothing logged."}</span> : text}
      </pre>
    </div>
  );
}
