/**
 * The conversation stores, live, as one of hyperlight's article figures: the scene (every
 * machine's store, the feeds into the server's, the mirror's copy), its tabs (replication, the
 * server's store, the mirror, upkeep), each with what it is and its facts as they stand now.
 */
import * as React from "react";
import type { ConvoStatus, ConvoStore } from "@/api/types";
import { agoText, countText, sizeText, stateText, storesOf, storesScene, toneOf, type Part, type Scene, type Tone } from "./scene";

const TONE: Record<Exclude<Tone, "neutral">, string> = { pass: "#a8c9bc", fail: "#d5a4ad", pending: "#aab8d2", error: "#d8bb95" };
const toneColor = (t: Tone | undefined) => (t && t !== "neutral" ? TONE[t] : undefined);

/** How much of each part a tab brings forward. */
const EMPHASIS: Record<Part, number>[] = [
  { runners: 1, feeds: 1, server: 1, mirror: 0.8, upkeep: 0.7, all: 1 },
  { runners: 0.5, feeds: 0.45, server: 1, mirror: 0.5, upkeep: 1, all: 1 },
  { runners: 0.5, feeds: 0.45, server: 0.85, mirror: 1, upkeep: 0.6, all: 1 },
  { runners: 0.75, feeds: 0.45, server: 1, mirror: 0.9, upkeep: 1, all: 1 },
];
const PARTS: Part[] = ["runners", "feeds", "server", "mirror", "upkeep", "all"];

interface Step {
  label: string;
  title: string;
  description: string;
  facts: { label: string; value: string; tone?: Exclude<Tone, "neutral"> }[];
}

const rate = (v: number | null | undefined, unit = "/s") => (v == null ? "—" : `${v < 10 ? v.toFixed(1) : Math.round(v).toLocaleString("en-US")}${unit}`);
const bytesRate = (v: number | null | undefined) => (v == null ? "—" : `${sizeText(v)}/s`);

/** About the `q`th quantile of the waits counted (ms), from their power-of-two buckets. */
export function waitQuantile(waits: number[] | undefined, q: number): number | null {
  const total = (waits ?? []).reduce((a, b) => a + b, 0);
  if (!waits || total === 0) return null;
  const want = Math.max(1, Math.ceil(total * q));
  let seen = 0;
  for (let i = 0; i < waits.length; i++) {
    seen += waits[i]!;
    if (seen >= want) return 2 ** i;
  }
  return 2 ** (waits.length - 1);
}

function upkeepText(s: ConvoStore | null): { text: string; tone?: Exclude<Tone, "neutral"> } {
  const up = s?.report?.file.upkeep;
  if (!up) return { text: "—" };
  switch (up.state) {
    case "compacting": {
      const pct = up.of > 0 ? Math.min(100, Math.round((up.copied / up.of) * 100)) : 0;
      const what = up.phase === "copy" ? `copying ${up.table ?? "rows"}` : up.phase === "catch_up" ? "catching up" : up.phase === "count" ? "counting the copy" : "swapping";
      return { text: `making it compact: ${what}, ${pct}%`, tone: "pending" };
    }
    case "squeezing":
      return { text: "compressing older lines", tone: "pending" };
    case "pruning":
      return { text: `letting go of old transcripts (${up.lines.toLocaleString("en-US")} lines)`, tone: "pending" };
    default:
      return { text: "nothing to do" };
  }
}

/** Each tab, with its facts as they stand now. */
function stepsOf(status: ConvoStatus): Step[] {
  const now = status.at;
  const { server, runners, mirrors } = storesOf(status);
  const withStore = runners.filter((s) => s.id);
  const without = runners.filter((s) => !s.id);
  const synced = withStore.filter((s) => s.state === "in_sync").length;
  const behind = withStore.reduce((a, s) => a + (s.connected ? (s.feed?.behind ?? 0) : 0), 0);
  const batches = withStore.reduce((a, s) => a + (s.rates.batches ?? 0), 0);
  const wire = withStore.reduce((a, s) => a + (s.rates.wire ?? 0), 0);
  const f = server?.report?.file;
  const r = server?.report;
  const p99 = waitQuantile(r?.waits, 0.99);
  const mirror = mirrors[0];
  const copy = mirror?.report?.file;
  const restore = status.restore;
  const compacted = f?.compacted;
  const pruned = f?.pruned;
  const up = upkeepText(server);
  const busyNow = runners.concat(mirrors).filter((s) => s.report && s.report.file.upkeep.state !== "idle");
  return [
    {
      label: "Replication",
      title: "Every machine's store reaches the server",
      description:
        "Each runner keeps its sessions in a store of its own and feeds the server what it wrote after the number the server has, a batch at a time, two ahead of what the server acknowledged. A light runs on a cable while batches move on it.",
      facts: [
        { label: "In sync", value: `${synced} of ${withStore.length} stores`, tone: synced === withStore.length && withStore.length > 0 ? "pass" : undefined },
        { label: "Rows to come", value: behind > 0 ? countText(behind) : "none", tone: behind > 0 ? "pending" : "pass" },
        { label: "Batches", value: rate(batches) },
        { label: "On the wire", value: bytesRate(wire) },
        { label: "Machines without a store", value: without.length ? without.map((s) => s.machine).join(", ") : "none", tone: without.some((s) => s.connected) ? "error" : undefined },
      ],
    },
    {
      label: "Server store",
      title: "conversations.db, on the server",
      description:
        "One writer thread owns the file: what sessions and feeds send is committed every 50 ms, and reads take their turn between commits. Every write gets the next number; a feed sends what came after the number its taker has.",
      facts: [
        { label: "File", value: f ? sizeText(f.bytes) : "—" },
        { label: "Free in it", value: f ? `${sizeText(f.free)} (${f.bytes ? Math.round((f.free / f.bytes) * 100) : 0}%)` : "—" },
        { label: "Sessions", value: f?.sessions != null ? f.sessions.toLocaleString("en-US") : "—" },
        { label: "Writes", value: server ? `${rate(server.rates.writes)} · #${countText(f?.top ?? 0)}` : "—" },
        { label: "Writer busy", value: server?.rates.busy != null ? `${Math.round(server.rates.busy * 100)}%` : "—", tone: (server?.rates.busy ?? 0) > 0.8 ? "error" : undefined },
        { label: "Reads wait (p99)", value: p99 == null ? "—" : `under ${p99 < 1000 ? `${p99} ms` : `${p99 / 1000} s`}`, tone: (p99 ?? 0) >= 1024 ? "error" : undefined },
        { label: "Dropped · timed out", value: r ? `${r.dropped} · ${r.timeouts}` : "—", tone: r && (r.dropped > 0 || r.timeouts > 0) ? "fail" : undefined },
      ],
    },
    {
      label: "Mirror",
      title: mirror ? `A copy of the server's store on ${mirror.machine}` : "A copy of the server's store on another machine",
      description:
        "The server feeds a runner its whole store, the way runners feed it theirs, so a conversation outlives the server's disk. If the server's store is lost, the copy comes back the same way and fills in what it lacks.",
      facts: mirror
        ? [
            { label: "Mirror", value: mirror.machine },
            { label: "State", value: stateText(mirror, now), tone: toneOf(mirror.state) === "neutral" ? undefined : (toneOf(mirror.state) as Exclude<Tone, "neutral">) },
            { label: "It has", value: mirror.mirror?.ship.acked != null ? `#${countText(mirror.mirror.ship.acked)}` : "—" },
            { label: "Last batch", value: mirror.mirror?.ship.last_sent ? `${agoText(now - mirror.mirror.ship.last_sent)} ago` : "—" },
            { label: "Its copy", value: copy ? sizeText(copy.bytes) : "—" },
            {
              label: "Restore",
              value: restore.state === "idle" ? "none under way" : restore.state === "failed" ? `failed: ${restore.error}` : `${restore.state} (${restore.from})`,
              tone: restore.state === "failed" ? "fail" : restore.state === "idle" ? undefined : "pending",
            },
          ]
        : [
            { label: "Mirrors", value: status.mirrors_wanted > 0 ? `${status.mirrors_wanted} wanted, none chosen yet` : "turned off ([conversations] mirrors = 0)", tone: "error" },
            { label: "Restore", value: restore.state === "idle" ? "none under way" : restore.state },
          ],
    },
    {
      label: "Upkeep",
      title: "Each store looks after its own file",
      description:
        "While it has nothing else to do, a store compresses older transcript lines, lets go of the transcripts of agents that ended long ago, and once a third of its file is free makes the file compact, a few MB at a time, so reads never wait long.",
      facts: [
        { label: "Server's store", value: up.text, tone: up.tone },
        { label: "Last made compact", value: compacted ? `${sizeText(compacted.before)} → ${sizeText(compacted.after)}, ${agoText(now - compacted.at)} ago` : "not since kiln started" },
        { label: "Old transcripts let go", value: pruned ? `${pruned.lines.toLocaleString("en-US")} lines, ${agoText(now - pruned.at)} ago` : "not since kiln started" },
        { label: "Others at it now", value: busyNow.length ? busyNow.map((s) => `${s.machine}: ${upkeepText(s).text}`).join("; ") : "none" },
      ],
    },
  ];
}

/** Hyperlight's materials (components/proof-work/proof-scene.tsx): a surface lit from the top left. */
const MATERIALS: [string, string, string, string][] = [
  ["paper", "#6b7c89", "#1c2631", "#354250"],
  ["circuit", "#344d50", "#111d25", "#253342"],
  ["metal", "#6c788b", "#222c3a", "#42475b"],
  ["silicon", "#303747", "#0c111b", "#242033"],
  ["shadow", "#020407", "#020407", "#020407"],
  ["emissive", "#b7e3d6", "#669daa", "#b8b0dc"],
];

function SceneSvg({ scene, selection, portrait, onPick }: { scene: Scene; selection: number; portrait: boolean; onPick: (key: string) => void }) {
  const prefix = "cv-" + React.useId().replace(/[^a-zA-Z0-9_-]/g, "");
  const emphasis = EMPHASIS[selection] ?? EMPHASIS[0]!;
  return (
    <svg className={portrait ? "cv-portrait" : "cv-landscape"} viewBox={`0 0 ${scene.width} ${scene.height}`} fill="none" aria-hidden="true" focusable="false">
      <defs>
        <linearGradient id={`${prefix}-prism`} gradientUnits="userSpaceOnUse" x1="-100" y1="80" x2={scene.width - 40} y2={scene.height - 160}>
          <stop stopColor="#a4cabe" />
          <stop offset=".34" stopColor="#9db8d5" />
          <stop offset=".68" stopColor="#b7a2c9" />
          <stop offset="1" stopColor="#d0bda0" />
        </linearGradient>
        <linearGradient id={`${prefix}-glass`} x1="0" y1="0" x2=".8" y2="1">
          <stop stopColor="#b8cbd7" stopOpacity=".24" />
          <stop offset=".22" stopColor="#7c91a1" stopOpacity=".02" />
          <stop offset=".63" stopColor="#c0bdd5" stopOpacity=".12" />
          <stop offset="1" stopColor="#a4bacb" stopOpacity=".025" />
        </linearGradient>
        {MATERIALS.map(([name, a, b, c]) => (
          <linearGradient key={name} id={`${prefix}-${name}`} x1="0" y1="0" x2=".85" y2="1">
            <stop stopColor={a} />
            <stop offset=".48" stopColor={b} />
            <stop offset="1" stopColor={c} />
          </linearGradient>
        ))}
        <radialGradient id={`${prefix}-halo`}>
          <stop stopColor="#94acc1" stopOpacity=".055" />
          <stop offset="1" stopColor="#94acc1" stopOpacity="0" />
        </radialGradient>
      </defs>
      <ellipse cx={scene.width / 2} cy={scene.height * 0.42} rx={scene.width * 0.48} ry={scene.height * 0.38} fill={`url(#${prefix}-halo)`} />
      {PARTS.map((part) => (
        <g key={part} className="cv-part" style={{ opacity: emphasis[part] }}>
          {scene.paths
            .filter((p) => p.part === part)
            .map((p) => {
              const fill = p.material ? `url(#${prefix}-${p.material})` : p.kind === "glass" ? `url(#${prefix}-glass)` : "none";
              return (
                <path
                  key={p.id}
                  className={`cv-${p.kind}`}
                  d={p.d}
                  // Per paint, not per element: an element's own opacity is a layer of its own, which
                  // Chromium paints wrong under a gradient fill (a dark band over its box).
                  strokeOpacity={(p.material === "shadow" ? 0 : p.material ? p.opacity * 0.85 : p.opacity).toFixed(3)}
                  fillOpacity={fill === "none" ? undefined : p.opacity.toFixed(3)}
                  fill={fill}
                  stroke={`url(#${prefix}-prism)`}
                  style={{ stroke: toneColor(p.tone) }}
                  vectorEffect="non-scaling-stroke"
                  pathLength={p.kind === "light" ? 200 : undefined}
                  strokeDasharray={p.dash}
                />
              );
            })}
          {scene.labels
            .filter((l) => l.part === part)
            .map((l) => (
              <text key={l.id} className={`cv-text-${l.kind}`} x={l.x} y={l.y} textAnchor={l.anchor ?? "middle"} style={{ fill: toneColor(l.tone) }}>
                {l.text}
              </text>
            ))}
        </g>
      ))}
      {scene.hits.map((h) => (
        <rect key={h.key} className="cv-hit" x={h.x} y={h.y} width={Math.max(0, h.w)} height={Math.max(0, h.h)} rx="6" onClick={() => onPick(h.key)}>
          <title>{h.title}</title>
        </rect>
      ))}
    </svg>
  );
}

/** The figure. `onPick`: a store clicked in the scene (its dashboard opens). */
export function StoresFigure({ status, onPick }: { status: ConvoStatus; onPick: (key: string) => void }) {
  const [selected, setSelected] = React.useState(0);
  const steps = stepsOf(status);
  const landscape = React.useMemo(() => storesScene(status, false), [status]);
  const portrait = React.useMemo(() => storesScene(status, true), [status]);
  const id = React.useId().replace(/[^a-zA-Z0-9_-]/g, "");
  const keyDown = (e: React.KeyboardEvent<HTMLButtonElement>, i: number) => {
    const next = e.key === "ArrowRight" ? (i + 1) % steps.length : e.key === "ArrowLeft" ? (i + steps.length - 1) % steps.length : e.key === "Home" ? 0 : e.key === "End" ? steps.length - 1 : -1;
    if (next < 0) return;
    e.preventDefault();
    setSelected(next);
    e.currentTarget.parentElement?.querySelectorAll<HTMLButtonElement>("button")[next]?.focus();
  };
  return (
    <figure className="cv-figure" aria-labelledby={`${id}-title`}>
      <header className="cv-header">
        <p className="cv-eyebrow">CONVERSATIONS / ONE STORE PER MACHINE</p>
        <h3 className="cv-title" id={`${id}-title`}>
          Replication, live
        </h3>
      </header>
      <div className="cv-body">
        <div className="cv-stage" style={{ "--cv-landscape": `${landscape.width} / ${landscape.height}`, "--cv-portrait": `${portrait.width} / ${portrait.height}` } as React.CSSProperties}>
          <SceneSvg scene={landscape} selection={selected} portrait={false} onPick={onPick} />
          <SceneSvg scene={portrait} selection={selected} portrait onPick={onPick} />
        </div>
        <div className="cv-side">
          <div className="cv-tabs" role="tablist" aria-label="The conversation stores">
            {steps.map((s, i) => (
              <button
                key={s.label}
                type="button"
                role="tab"
                id={`${id}-tab-${i}`}
                aria-controls={`${id}-panel-${i}`}
                aria-selected={selected === i}
                tabIndex={selected === i ? 0 : -1}
                onClick={() => setSelected(i)}
                onKeyDown={(e) => keyDown(e, i)}
              >
                {s.label}
              </button>
            ))}
          </div>
          <div className="cv-panels">
            {steps.map((s, i) => (
              <section
                key={s.label}
                className="cv-panel"
                role="tabpanel"
                id={`${id}-panel-${i}`}
                aria-labelledby={`${id}-tab-${i}`}
                aria-hidden={selected !== i}
                inert={selected !== i}
                data-active={String(selected === i)}
              >
                <h4>{s.title}</h4>
                <p>{s.description}</p>
                <dl className="facts cv-facts">
                  {s.facts.map((f) => (
                    <div key={f.label} data-tone={f.tone}>
                      <dt>{f.label}</dt>
                      <dd>{f.value}</dd>
                    </div>
                  ))}
                </dl>
              </section>
            ))}
          </div>
        </div>
      </div>
      <figcaption className="cv-caption">
        Live from this server, read again every 2 seconds. A light runs on a cable while batches move on it, and on a store&apos;s file while it writes; a cable drawn faint is down. Rows to come
        compare a store&apos;s last write with what the server has of it. Click a store for its dashboard.
      </figcaption>
    </figure>
  );
}
