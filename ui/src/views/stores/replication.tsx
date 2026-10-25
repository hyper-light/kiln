/**
 * Replication, live: the conversation stores as databases (each runner's, the server's, the
 * mirror's copy) with the cables between them, a light running on a cable while batches move on it
 * and idle cables still; what a picked store is doing; and the replication log, every batch the
 * board saw move and everything that happened to the stores, live, searchable and downloadable.
 */
import * as React from "react";
import { Download } from "lucide-react";
import { api } from "@/api/client";
import type { ConvoLogLine, ConvoStatus } from "@/api/types";
import { Button } from "@/components/ui/button";
import { EmptyState, SearchField, SectionHeading } from "@/components/ui/misc";
import { toast } from "@/components/ui/toast";
import { fileStamp, saveFile } from "@/lib/download";
import { errorMessage } from "@/lib/hooks";
import { useSuspenseResource } from "@/lib/resource";
import { cn } from "@/lib/utils";
import { cylinder, frontArc, healthOf, metricsOf, replicationScene, wall, type Cable, type Node, type Plate, type ReplicationScene } from "./replication-scene";
import { countText, sizeText } from "./scene";
import "./replication.css";

/** A database, as the figure draws one: a glass cylinder, its rings, its cap, and on the server's
 * and the mirror's the ring its commits light. */
function Database({ plate, big, id }: { plate: Plate; big: boolean; id: string }) {
  const d = cylinder(plate);
  const bottom = d.top + d.depth;
  const prism = `url(#${id}-prism)`;
  return (
    <g>
      <ellipse cx={d.x + 2} cy={bottom + (big ? 7 : 4)} rx={d.rx + 1} ry={d.ry} className="rp-shadow" />
      <path d={wall(d)} fill={`url(#${id}-wall)`} stroke={prism} className="rp-facet" strokeOpacity={0.52} />
      {[0.3, 0.64, 1].map((level) => (
        <g key={level}>
          <ellipse cx={d.x} cy={d.top + d.depth * level} rx={d.rx} ry={d.ry} className="rp-rear" />
          <path d={frontArc(d, level)} className="rp-ring" stroke={prism} strokeWidth={level === 1 ? 0.85 : 0.65} strokeOpacity={level === 1 ? 0.7 : 0.46} />
        </g>
      ))}
      <ellipse cx={d.x} cy={d.top} rx={d.rx} ry={d.ry} fill={`url(#${id}-cap)`} stroke={prism} className="rp-facet" strokeOpacity={0.75} />
      <ellipse cx={d.x} cy={d.top} rx={d.rx * 0.78} ry={d.ry * 0.73} className="rp-inner" />
      <path d={frontArc(d, 0)} className="rp-ring" stroke={prism} strokeWidth={0.95} strokeOpacity={0.8} />
      {big && (
        <>
          {[0.45, 0.72, 2.35].map((a) => {
            const x = d.x + Math.cos(a) * d.rx;
            const y = d.top + d.depth * 0.64 + Math.sin(a) * d.ry;
            return <path key={a} d={`M${x.toFixed(2)} ${(y - 2).toFixed(2)} L${x.toFixed(2)} ${(y + 2).toFixed(2)}`} className="rp-etch" />;
          })}
          <path d={frontArc(d, 0.64)} className="rp-commit" />
        </>
      )}
    </g>
  );
}

/** A cable: its line, its direction, rows to come waiting at its start, and while batches move on
 * it, light flowing along it, about one comet arriving a batch. */
function CableView({ cable, dim, delay, kind }: { cable: Cable; dim: boolean; delay: number; kind: "in" | "out" }) {
  const style = cable.period ? ({ "--rp-period": `${cable.period}s`, "--rp-delay": `${-delay}s` } as React.CSSProperties) : undefined;
  return (
    <g className="rp-cable" data-tone={cable.tone} data-dim={dim || undefined}>
      <path d={cable.d} className="rp-cable-shadow" />
      <path d={cable.d} className="rp-cable-line" />
      <path d={cable.arrow} className="rp-arrow" />
      {cable.marks.map((m) => (
        <path key={m} d={m} className="rp-mark" />
      ))}
      {cable.period && (
        <g className="rp-light" data-kind={kind} data-warn={cable.tone === "warn" || undefined} style={style} aria-hidden="true">
          <path d={cable.d} pathLength={100} className="rp-light-halo" />
          <path d={cable.d} pathLength={100} className="rp-light-core" />
        </g>
      )}
    </g>
  );
}

function NodeView({ node, selected, onSelect, id }: { node: Node; selected: boolean; onSelect: (key: string) => void; id: string }) {
  return (
    <g
      className="rp-node"
      role="button"
      tabIndex={0}
      aria-pressed={selected}
      aria-label={node.title}
      data-selected={selected || undefined}
      onClick={() => onSelect(node.key)}
      onKeyDown={(e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          onSelect(node.key);
        }
      }}
    >
      <rect className="rp-hit" x={node.hit.x} y={node.hit.y} width={node.hit.w} height={node.hit.h} rx={5} />
      <Database plate={node.plate} big={node.big} id={id} />
      {node.heading && (
        <text className="rp-heading" x={node.heading.at[0]} y={node.heading.at[1]} textAnchor={node.heading.anchor}>
          {node.heading.text}
        </text>
      )}
      <text className={node.big ? "rp-node-name" : "rp-ink"} x={node.name.at[0]} y={node.name.at[1]} textAnchor={node.name.anchor}>
        {node.name.text}
      </text>
      <text className="rp-sub" data-tone={node.sub.tone} x={node.sub.at[0]} y={node.sub.at[1]} textAnchor={node.sub.anchor}>
        {node.sub.text}
      </text>
    </g>
  );
}

function SceneView({ scene, className, selected, onSelect }: { scene: ReplicationScene; className: string; selected: string | null; onSelect: (key: string) => void }) {
  const id = "rp-" + React.useId().replace(/[^a-zA-Z0-9_-]/g, "");
  const { server, mirror } = scene;
  // A runner's store picked: the other runners' cables step back.
  const source = selected && scene.sources.some((s) => s.key === selected) ? selected : null;
  const halo = (p: Plate, rx: number, ry: number) => <ellipse cx={p.x} cy={p.y} rx={rx} ry={ry} fill={`url(#${id}-halo)`} />;
  const portrait = scene.width < 500;
  return (
    <svg
      className={cn("rp-scene", className)}
      viewBox={`0 0 ${scene.width} ${scene.height}`}
      role="group"
      aria-label="Replication: each runner's store sends batches to the server's; the server's sends what it committed to its mirror. Pick a store for what it is doing."
    >
      <defs>
        <linearGradient id={`${id}-prism`} x1="0" y1="0" x2="1" y2=".8">
          <stop stopColor="#a8d2c7" />
          <stop offset=".4" stopColor="#a6bfdc" />
          <stop offset=".75" stopColor="#b5a5cf" />
          <stop offset="1" stopColor="#d3bda7" />
        </linearGradient>
        <linearGradient id={`${id}-wall`} x1="0" y1="0" x2="1" y2=".12">
          <stop stopColor="#101925" />
          <stop offset=".24" stopColor="#283746" />
          <stop offset=".55" stopColor="#1c2938" />
          <stop offset="1" stopColor="#10141f" />
        </linearGradient>
        <linearGradient id={`${id}-cap`} x1="0" y1="0" x2=".8" y2="1">
          <stop stopColor="#809aaa" stopOpacity=".24" />
          <stop offset=".42" stopColor="#344755" stopOpacity=".28" />
          <stop offset="1" stopColor="#70738c" stopOpacity=".2" />
        </linearGradient>
        <radialGradient id={`${id}-halo`}>
          <stop stopColor="#91abc8" stopOpacity=".055" />
          <stop offset="1" stopColor="#91abc8" stopOpacity="0" />
        </radialGradient>
      </defs>
      {halo(server.plate, portrait ? 100 : 200, portrait ? 95 : 145)}
      {mirror && halo(mirror.plate, portrait ? 80 : 125, portrait ? 60 : 110)}
      {scene.headings.map((h) => (
        <text key={h.text} className="rp-heading" x={h.at[0]} y={h.at[1]} textAnchor={h.anchor}>
          {h.text}
        </text>
      ))}
      {scene.sources.map((s, i) => (
        <CableView key={s.key} cable={s.cable} dim={source !== null && source !== s.key} delay={(i * 0.37) % (s.cable.period ?? 1)} kind="in" />
      ))}
      {mirror && <CableView cable={mirror.cable} dim={false} delay={0.2} kind="out" />}
      {scene.sources.map((s) => (
        <NodeView key={s.key} node={s} selected={selected === s.key} onSelect={onSelect} id={id} />
      ))}
      <NodeView node={server} selected={selected === server.key} onSelect={onSelect} id={id} />
      {mirror && <NodeView node={mirror} selected={selected === mirror.key} onSelect={onSelect} id={id} />}
    </svg>
  );
}

const rate = (v: number | null | undefined) => (v == null ? "—" : `${v < 10 ? v.toFixed(1) : Math.round(v).toLocaleString("en-US")}`);

/** What the readout says: of a picked store, or of replication as a whole. */
function readoutOf(status: ConvoStatus, selected: string | null): { title: string; note: string } {
  const s = selected ? status.stores.find((x) => x.key === selected) : null;
  if (s?.role === "server") {
    const f = s.report?.file;
    return { title: `Server · conversations.db · ${f ? sizeText(f.bytes) : "—"}`, note: `Takes each runner's records, ${rate(s.rates.writes)} writes/s; feeds the whole store to its mirror.` };
  }
  if (s?.role === "mirror") {
    const f = s.report?.file;
    const behind = s.mirror?.behind ?? 0;
    return {
      title: `Mirror · ${s.machine} · ${f ? sizeText(f.bytes) : "—"}`,
      note: !s.connected
        ? "Offline. The server's store waits to send it what it lacks."
        : behind > 0
          ? `${countText(behind)} rows behind the server's store, catching up.`
          : "A current copy of the server's store, on another machine.",
    };
  }
  if (s) {
    const f = s.report?.file;
    const behind = s.feed?.behind ?? 0;
    return {
      title: `${s.machine} · ${f ? sizeText(f.bytes) : "keeps no store"}`,
      note: !s.connected
        ? "Offline. Its history stays kept; its cable comes back with the runner."
        : `${rate(s.rates.writes)} writes/s · ${countText(behind)} to come · ${s.state === "behind" ? "stalled" : behind > 0 ? "catching up" : "in sync"}`,
    };
  }
  const m = metricsOf(status);
  const health = healthOf(status);
  if (m.pending > 0) return { title: `${countText(m.pending)} rows still to arrive.`, note: "New writes go on while the backlog drains." };
  if (health.tone === "off") return { title: `${health.text}. Its history stays kept.`, note: "The others go on sending; the mirror stays current." };
  return { title: "Local writes become one shared history.", note: "Batches arrive, commit, then go on to the mirror." };
}

/** The figure. `selected`: the store picked (here or in the table), its dashboard shown below. */
export function ReplicationFigure({ status, selected, onSelect }: { status: ConvoStatus; selected: string | null; onSelect: (key: string | null) => void }) {
  const landscape = React.useMemo(() => replicationScene(status, false), [status]);
  const portrait = React.useMemo(() => replicationScene(status, true), [status]);
  const health = healthOf(status);
  const m = metricsOf(status);
  const said = readoutOf(status, selected);
  const id = React.useId();
  const pick = (key: string) => onSelect(selected === key ? null : key);
  return (
    <>
      <figure className="rp-figure" aria-labelledby={`${id}-title`}>
        <header className="rp-head">
          <div>
            <p className="rp-eyebrow">Conversation stores</p>
            <h3 className="rp-title" id={`${id}-title`}>
              Replication
            </h3>
          </div>
          <span className="rp-status" data-tone={health.tone} role="status">
            {health.text}
          </span>
        </header>
        <div className="rp-stage">
          <SceneView scene={landscape} className="rp-landscape" selected={selected} onSelect={pick} />
          <SceneView scene={portrait} className="rp-portrait" selected={selected} onSelect={pick} />
        </div>
        <div className="rp-readout">
          <svg className="rp-readout-mark" viewBox="0 0 24 24" fill="none" aria-hidden="true">
            <path d="m4 9 8-4 8 4-8 4-8-4Zm0 5 8 4 8-4M4 18l8 4 8-4" stroke="currentColor" strokeWidth=".9" />
          </svg>
          <div className="rp-readout-text" aria-live="polite">
            <span className="rp-readout-title">{said.title}</span>
            <span className="rp-readout-note">{said.note}</span>
          </div>
          {selected ? (
            <button type="button" className="rp-reset" onClick={() => onSelect(null)}>
              All stores
            </button>
          ) : (
            <div className="rp-legend" aria-hidden="true">
              <span>
                <i /> Batch
              </span>
              <span>
                <i /> Committed
              </span>
            </div>
          )}
        </div>
        <dl className="rp-metrics">
          <div>
            <dt>Sources in sync</dt>
            <dd>
              {m.synced}
              <span className="rp-slash">/</span>
              {m.of}
            </dd>
          </div>
          <div>
            <dt>Pending rows</dt>
            <dd data-tone={m.pending > 0 ? "warn" : undefined}>{m.pending.toLocaleString("en-US")}</dd>
          </div>
          <div>
            <dt>Mirror copy</dt>
            <dd className="rp-word" data-tone={m.mirror.tone}>
              {m.mirror.text}
            </dd>
          </div>
        </dl>
      </figure>
      <p className="rp-foot">
        <span>Live from this server, every 2 seconds. Pick a store to see its flow and its dashboard.</span>
        <span>Light follows batches · idle cables stay still</span>
      </p>
    </>
  );
}

/** Lines shown at once; Download takes every line that matches. */
const SHOWN = 400;
const WORD: Record<ConvoLogLine["kind"], string> = { batch: "batch", restore: "restore", ship: "to mirror", note: "note" };

const clock = (ms: number) => new Date(ms).toLocaleTimeString("en-GB", { hour12: false });
const lineText = (l: ConvoLogLine) => `${new Date(l.at).toISOString()}  ${WORD[l.kind].padEnd(9)}  ${l.key.padEnd(24)}  ${l.text}`;

/** The replication log: live (read again every 4 s), searched on the server as you type, and
 * downloaded whole (every line that matches). */
export function ReplicationLog() {
  const [q, setQ] = React.useState("");
  const words = React.useDeferredValue(q.trim());
  const log = useSuspenseResource(`conversations/log/${words}`, (sig) => api.convoLog({ q: words || undefined, limit: SHOWN }, sig), { pollMs: 4_000 }).data;
  const [saving, setSaving] = React.useState(false);
  const download = async () => {
    setSaving(true);
    try {
      const all = await api.convoLog({ q: words || undefined, limit: 5_000 });
      saveFile(`kiln-replication-log-${fileStamp()}.txt`, all.lines.map(lineText).join("\n") + "\n");
    } catch (e) {
      toast.error("Couldn't download the replication log", errorMessage(e));
    } finally {
      setSaving(false);
    }
  };
  const count = words ? `${log.matched.toLocaleString("en-US")} of ${log.total.toLocaleString("en-US")}` : log.total.toLocaleString("en-US");
  return (
    <section className="rp-log">
      <SectionHeading label="Log" title="Replication log" />
      <div className="log-tools">
        <SearchField className="min-w-0 flex-1" label="Search the replication log" placeholder="Search by machine, store, or words…" value={q} onChange={setQ} />
        <span className="log-count">{count} lines</span>
        <Button variant="ghost" size="sm" disabled={saving || log.lines.length === 0} title="Download every line that matches" onClick={() => void download()}>
          <Download /> {saving ? "Downloading…" : "Download"}
        </Button>
      </div>
      {log.lines.length === 0 ? (
        <EmptyState title={words ? "No lines match." : "Nothing has moved yet."}>{!words && <p>Lines come as the stores send batches, sampled every 5 seconds.</p>}</EmptyState>
      ) : (
        <ol className="rp-log-lines" aria-label="Replication log, newest first">
          {log.lines.map((l) => (
            <li key={`${l.at}|${l.key}|${l.kind}|${l.text}`} data-kind={l.kind} data-level={l.level}>
              <time dateTime={new Date(l.at).toISOString()} title={new Date(l.at).toLocaleString()}>
                {clock(l.at)}
              </time>
              <span className="rp-log-kind">{WORD[l.kind]}</span>
              <span className="rp-log-text">{l.text}</span>
            </li>
          ))}
        </ol>
      )}
      {log.lines.length > 0 && log.lines.length < (words ? log.matched : log.total) && (
        <p className="rp-log-more">The newest {log.lines.length.toLocaleString("en-US")} shown; Download has them all.</p>
      )}
    </section>
  );
}
