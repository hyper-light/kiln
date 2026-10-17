/**
 * One object of an app, in the page: what it is and how it stands (kind, namespace, age, ready
 * counts, images, owners, conditions, labels), its live and desired manifests, what a sync would
 * change in it, its events, and — for pods and what runs them — its logs, read once or followed
 * as they are written. A select chooses which; nothing opens over the page.
 */
import * as React from "react";
import { X } from "lucide-react";

import { cdApi, type CdLogQuery } from "@/api/cd";
import { wsUrl } from "@/api/client";
import type { CdLogLine, CdNode, CdResourceDetail } from "@/api/types";
import { Button } from "@/components/ui/button";
import { CopyButton, ErrorState, SelectField, SkeletonRows } from "@/components/ui/misc";
import { errorMessage, useNow, useResource } from "@/lib/hooks";
import { FileDiffView } from "@/views/harness/diff";
import { age, compareText } from "./model";
import { HealthPair, KindBadge, StatePair, SyncPair, objectLabel } from "./parts";

type Show = "summary" | "live" | "desired" | "diff" | "events" | "logs";

/** Kinds that run pods, whose logs are theirs. */
const RUNS_PODS = new Set(["Pod", "Deployment", "ReplicaSet", "StatefulSet", "DaemonSet", "Job", "CronJob"]);

const when = (t: string | null | undefined) => {
  if (!t) return "";
  const d = Date.parse(t);
  return Number.isNaN(d) ? t : new Date(d).toLocaleString();
};

export function ResourceDetails({
  appId,
  node,
  tick,
  onClose,
  onSelect,
}: {
  appId: string;
  node: CdNode;
  /** Bumped when the app changes, so what is shown is read again. */
  tick: number;
  onClose: () => void;
  /** Go to another node (an owner). */
  onSelect: (id: string) => void;
}) {
  const now = useNow();
  const key = React.useMemo(() => ({ group: node.group, kind: node.kind, namespace: node.namespace, name: node.name }), [node.group, node.kind, node.namespace, node.name]);
  const detail = useResource((s) => cdApi.resource(appId, key, s), [appId, key, tick]);
  const logs = RUNS_PODS.has(node.kind) && !node.missing;
  const options: { value: Show; label: string }[] = [
    { value: "summary", label: "Summary" },
    { value: "live", label: "Live manifest" },
    ...(node.managed ? [{ value: "desired" as const, label: "Desired manifest" }, { value: "diff" as const, label: "Diff" }] : []),
    { value: "events", label: "Events" },
    ...(logs ? [{ value: "logs" as const, label: "Logs" }] : []),
  ];
  const [show, setShow] = React.useState<Show>(node.kind === "Pod" && (node.health === "Degraded" || (node.restarts ?? 0) > 0) ? "logs" : "summary");
  const current = options.some((o) => o.value === show) ? show : "summary";
  const head = React.useRef<HTMLElement>(null);
  // Brought into view when opened, gently unless motion is reduced.
  React.useEffect(() => {
    const reduce = window.matchMedia("(prefers-reduced-motion: reduce)").matches || document.documentElement.dataset.motion === "paused";
    head.current?.scrollIntoView({ block: "nearest", behavior: reduce ? "auto" : "smooth" });
  }, [node.id]);

  return (
    <section ref={head} className="cd-details" aria-label={`${node.kind} ${node.name}`}>
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
        {[node.kind, node.namespace || "cluster-scoped", node.created_at ? `${age(node.created_at, now)} old` : "", node.node ? `on ${node.node}` : ""]
          .filter(Boolean)
          .join(" · ")}
      </p>
      <div className="cd-details-state">
        <HealthPair health={node.health} />
        {node.managed && !node.hook && node.sync && <SyncPair sync={node.sync} />}
        {node.managed ? <span className="cd-meta">managed</span> : <span className="cd-meta">live, owned by the app's objects</span>}
      </div>
      {(node.message || node.why) && <p className="cd-details-why">{node.why ?? node.message}</p>}
      <SelectField label="Show" value={current} onChange={(v) => setShow(v as Show)} options={options} className="cd-details-show" />
      <div className="cd-details-body">
        {current === "logs" ? (
          <Logs appId={appId} node={node} />
        ) : current === "events" ? (
          <Events appId={appId} node={node} tick={tick} />
        ) : detail.error && !detail.data ? (
          <ErrorState error={detail.error} onRetry={detail.reload} compact />
        ) : !detail.data ? (
          <SkeletonRows rows={4} cols={3} />
        ) : current === "summary" ? (
          <Summary d={detail.data} node={node} onSelect={onSelect} />
        ) : current === "live" ? (
          <Manifest text={detail.data.live} empty="It isn't in the cluster." name={`${node.name}.live.yaml`} />
        ) : current === "desired" ? (
          <Manifest text={detail.data.desired} empty="Git doesn't hold it any more." name={`${node.name}.yaml`} />
        ) : (
          <Diff d={detail.data} />
        )}
      </div>
    </section>
  );
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
      <pre className="cd-yaml">{text}</pre>
    </div>
  );
}

function Diff({ d }: { d: CdResourceDetail }) {
  if (!d.diff) return <p className="cd-empty">kiln doesn't manage it, so there is nothing to compare.</p>;
  if (d.diff.hunks.length === 0) return <p className="cd-empty">It matches git: a sync changes nothing in it.</p>;
  return (
    <div className="cd-diffs">
      <p className="cd-meta">What a sync changes: − is what runs, + what it becomes. Fields the cluster fills in itself are left out.</p>
      <FileDiffView diff={{ path: objectLabel(d.key), hunks: d.diff.hunks }} label={objectLabel(d.key)} />
    </div>
  );
}

function Events({ appId, node, tick }: { appId: string; node: CdNode; tick: number }) {
  const now = useNow();
  const key = { group: node.group, kind: node.kind, namespace: node.namespace, name: node.name };
  const ev = useResource((s) => cdApi.events(appId, key, s), [appId, node.id, tick], { pollMs: 15_000 });
  if (ev.error && !ev.data) return <ErrorState error={ev.error} onRetry={ev.reload} compact />;
  if (!ev.data) return <SkeletonRows rows={3} cols={3} />;
  const list = ev.data.events;
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

function Logs({ appId, node }: { appId: string; node: CdNode }) {
  const key = React.useMemo(() => ({ group: node.group, kind: node.kind, namespace: node.namespace, name: node.name }), [node.group, node.kind, node.namespace, node.name]);
  const [container, setContainer] = React.useState("");
  const [pod, setPod] = React.useState("");
  const [since, setSince] = React.useState("");
  const [tail, setTail] = React.useState("500");
  const [follow, setFollow] = React.useState(false);
  const [wrap, setWrap] = React.useState(true);
  const [previous, setPrevious] = React.useState(false);
  const [times, setTimes] = React.useState(false);
  const [lines, setLines] = React.useState<CdLogLine[]>([]);
  const [pods, setPods] = React.useState<string[]>([]);
  const [containers, setContainers] = React.useState<string[]>(() => node.containers.filter((c) => !c.init).map((c) => c.name));
  const [more, setMore] = React.useState(0);
  const [errors, setErrors] = React.useState<string[]>([]);
  const [state, setState] = React.useState<"loading" | "ready" | "live" | "ended" | "failed">("loading");
  const [nonce, setNonce] = React.useState(0);
  const box = React.useRef<HTMLPreElement>(null);
  const stick = React.useRef(true);

  const q: CdLogQuery = React.useMemo(
    () => ({
      container: container || undefined,
      pod: pod || undefined,
      since_seconds: since ? Number(since) : undefined,
      tail_lines: Number(tail),
      previous: previous || undefined,
    }),
    [container, pod, since, tail, previous],
  );

  // Read once (and when asked again).
  React.useEffect(() => {
    if (follow) return;
    const ctl = new AbortController();
    setState("loading");
    cdApi
      .logs(appId, key, q, ctl.signal)
      .then((r) => {
        setLines(r.lines);
        setPods(r.pods);
        setMore(r.more_pods);
        if (r.containers.length) setContainers(r.containers);
        setErrors(r.errors);
        setState("ready");
      })
      .catch((e: unknown) => {
        if (ctl.signal.aborted) return;
        setErrors([errorMessage(e)]);
        setState("failed");
      });
    return () => ctl.abort();
  }, [appId, key, q, follow, nonce]);

  // Followed: a WebSocket says each line as it is written.
  React.useEffect(() => {
    if (!follow) return;
    setLines([]);
    setErrors([]);
    setState("loading");
    stick.current = true;
    const ws = new WebSocket(wsUrl(cdApi.logsFollowPath(appId, key, q)));
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
  }, [appId, key, q, follow, nonce]);

  // Kept at the bottom while it is followed, unless you scrolled up to read.
  React.useLayoutEffect(() => {
    const el = box.current;
    if (el && stick.current) el.scrollTop = el.scrollHeight;
  }, [lines]);

  const many = pods.length > 1 && !pod;
  const stamp = (ts: string) => ts.replace("T", " ").replace(/\.\d+Z$/, "Z");
  const text = React.useMemo(
    () => lines.map((l) => [times && l.ts ? stamp(l.ts) : "", many ? l.pod : "", l.text].filter(Boolean).join("  ")).join("\n"),
    [lines, times, many],
  );
  const podOptions = [{ value: "", label: pods.length > 1 || more ? `All ${pods.length + more} pods` : "Its pod" }, ...pods.map((p) => ({ value: p, label: p }))];
  return (
    <div className="cd-logs">
      <div className="cd-logs-bar">
        {containers.length > 1 && (
          <SelectField
            label="Container"
            value={container || containers[0] || ""}
            onChange={setContainer}
            options={containers.map((c) => ({ value: c, label: c }))}
            className="cd-logs-select"
          />
        )}
        {node.kind !== "Pod" && pods.length > 1 && <SelectField label="Pod" value={pod} onChange={setPod} options={podOptions} className="cd-logs-select" />}
        <SelectField label="Since" value={since} onChange={setSince} options={SINCE} className="cd-logs-select" />
        <SelectField label="Lines" value={tail} onChange={setTail} options={TAIL} className="cd-logs-select" />
      </div>
      <div className="cd-logs-bar">
        <label className="cd-opt">
          <input type="checkbox" className="check" checked={follow} onChange={(e) => setFollow(e.target.checked)} /> Follow
        </label>
        <label className="cd-opt cd-opt-wrap">
          <input type="checkbox" className="check" checked={wrap} onChange={(e) => setWrap(e.target.checked)} /> Wrap
        </label>
        <label className="cd-opt">
          <input type="checkbox" className="check" checked={times} onChange={(e) => setTimes(e.target.checked)} /> Times
        </label>
        {!follow && (
          <label className="cd-opt" title="The run before the container last restarted">
            <input type="checkbox" className="check" checked={previous} onChange={(e) => setPrevious(e.target.checked)} /> Previous run
          </label>
        )}
        <span className="cd-logs-end">
          <span className="cd-meta" aria-live="polite">
            {state === "live" ? "Following" : state === "ended" ? "Ended" : state === "loading" ? "Reading…" : `${lines.length.toLocaleString()} lines`}
          </span>
          {!follow && (
            <Button size="xs" variant="ghost" onClick={() => setNonce((n) => n + 1)}>
              Read again
            </Button>
          )}
          <CopyButton text={text} className="cd-copy" />
        </span>
      </div>
      {errors.length > 0 && <p className="cd-warn">{errors.join("; ")}</p>}
      {more > 0 && <p className="cd-meta">Showing {pods.length} of {pods.length + more} pods: the newest.</p>}
      <pre
        ref={box}
        className="cd-log"
        data-wrap={wrap || undefined}
        tabIndex={0}
        aria-label="Log"
        onScroll={(e) => {
          const el = e.currentTarget;
          stick.current = el.scrollHeight - el.scrollTop - el.clientHeight < 24;
        }}
      >
        {lines.length === 0 ? <span className="cd-dim">{state === "loading" ? "Reading the log…" : "Nothing logged."}</span> : text}
      </pre>
    </div>
  );
}
