/**
 * System → Updates' figure, Rollout (the approved design study, system-updates): kiln's builds on
 * the left, its server in the middle, its runners on the right, the cables between them; on a phone
 * a tree, the server at the top and a branch to each runner. Light flows along a cable while
 * something moves on it, in CSS (held still as a mark under the motion setting and for reduced
 * motion): a runner's upgrade, the release pipeline building a platform; a faint pulse runs a
 * connected runner's cable now and then. Picking a machine or a build says what it is doing, and
 * lights its row or tile below.
 */
import * as React from "react";
import { cn } from "@/lib/utils";
import { MARK, rolloutScene, type BuildNode, type Cable, type IsoBox, type RolloutScene, type RunnerNode, type SceneBuild, type SceneRunner, type SceneServer, type ServerNode } from "./scene";
import { pick } from "./state";
import type { Readout, Tone } from "./words";

const LED: Record<Tone, string> = { ok: "#a8c9bc", warn: "#d0b591", bad: "#d6a2aa", run: "#b8b0d8", off: "#4a525e" };

export interface FigureProps {
  input: { builds: SceneBuild[]; server: SceneServer; runners: SceneRunner[] };
  status: { tone: Tone; text: string };
  readout: Readout;
  picked: string | null;
  metrics: { current: number; total: number; upgrading: number; newest: string | null; since: string | null };
}

export function RolloutFigure({ input, status, readout, picked, metrics }: FigureProps) {
  const title = React.useId();
  const landscape = rolloutScene(input, false);
  const portrait = rolloutScene(input, true);
  return (
    <section className="up-figure" aria-labelledby={title}>
      <header className="up-head">
        <div>
          <p className="up-eyebrow">Builds · server · runners</p>
          <h3 id={title} className="up-fig-title">
            Rollout
          </h3>
        </div>
        <span className="up-status" data-tone={status.tone} role="status">
          {status.text}
        </span>
      </header>
      <div className="up-stage">
        <SceneView scene={landscape} className="up-landscape" picked={picked} />
        <SceneView scene={portrait} className="up-portrait" picked={picked} />
      </div>
      <div className="up-readout">
        <svg className="up-readout-mark" viewBox="0 0 24 24" fill="none" aria-hidden="true">
          <path d="M6.4 17.5C3.4 13.4 6.9 5.2 12.1 3.9C17.6 2.6 22.1 8.1 20.4 12.9C18.7 17.7 11.4 22.1 8.2 20.4" stroke="currentColor" strokeWidth=".9" />
          <path d="M8.2 20.4C6.8 19.6 5.8 18.1 5.5 16.4" stroke="#bcb1d8" strokeWidth=".9" />
        </svg>
        <div className="up-readout-text" aria-live="polite">
          <span className="up-readout-title">{readout.title}</span>
          <span className="up-readout-note">{readout.note}</span>
        </div>
        {picked ? (
          <button type="button" className="up-reset" onClick={() => pick(null)}>
            Everything
          </button>
        ) : (
          <div className="up-legend" aria-hidden="true">
            <span>
              <i /> Download
            </span>
            <span>
              <i /> New build
            </span>
            <span>
              <i /> Current
            </span>
          </div>
        )}
      </div>
      <dl className="up-metrics">
        <div>
          <dt>Runners current</dt>
          <dd>
            {metrics.current}
            <span className="up-slash">/</span>
            {metrics.total}
          </dd>
        </div>
        <div>
          <dt>Upgrading</dt>
          <dd data-tone={metrics.upgrading ? "run" : undefined}>{metrics.upgrading}</dd>
        </div>
        <div>
          <dt>Newest kiln</dt>
          <dd>
            {metrics.newest ?? "—"}
            {metrics.since && <small>{metrics.since}</small>}
          </dd>
        </div>
      </dl>
    </section>
  );
}

function SceneView({ scene, className, picked }: { scene: RolloutScene; className: string; picked: string | null }) {
  const id = "up" + React.useId().replace(/[^a-zA-Z0-9_-]/g, "");
  const prism = `url(#${id}-prism)`;
  // A runner or a build picked: the other cables step back.
  const focus = picked && picked !== "server" ? picked : null;
  const dim = (key: string) => (focus !== null && focus !== key) || undefined;
  // Light flows along a cable while a platform builds or a runner upgrades, comet after comet; a
  // connected runner's cable carries a faint pulse every 6 to 8 s, the runners' out of step.
  const flows = [...scene.builds.map((b) => ({ key: b.key, cable: b.cable, kind: "build" })), ...scene.runners.map((r) => ({ key: r.key, cable: r.cable, kind: "down" }))].filter((l) => l.cable.lit);
  const pulses = scene.runners.filter((r) => r.pulse);
  return (
    <svg
      className={cn("up-scene", className)}
      viewBox={`0 0 ${scene.width} ${scene.height}`}
      style={{ aspectRatio: `${scene.width} / ${scene.height}` }}
      role="group"
      aria-label="Builds reach the server; each runner downloads its platform's build from it and switches over without stopping what it runs. Pick a machine or a build for what it is doing."
    >
      <defs>
        <linearGradient id={`${id}-prism`} x1="0" y1="0" x2="1" y2=".8">
          <stop stopColor="#a8d2c7" />
          <stop offset=".4" stopColor="#a6bfdc" />
          <stop offset=".75" stopColor="#b5a5cf" />
          <stop offset="1" stopColor="#d3bda7" />
        </linearGradient>
        <linearGradient id={`${id}-wall-l`} x1="0" y1="0" x2="1" y2=".2">
          <stop stopColor="#101925" />
          <stop offset=".45" stopColor="#253442" />
          <stop offset="1" stopColor="#151d29" />
        </linearGradient>
        <linearGradient id={`${id}-wall-r`} x1="0" y1="0" x2="1" y2=".3">
          <stop stopColor="#1a2532" />
          <stop offset="1" stopColor="#0e131c" />
        </linearGradient>
        <linearGradient id={`${id}-cap`} x1="0" y1="0" x2=".8" y2="1">
          <stop stopColor="#809aaa" stopOpacity=".26" />
          <stop offset=".45" stopColor="#344755" stopOpacity=".3" />
          <stop offset="1" stopColor="#70738c" stopOpacity=".22" />
        </linearGradient>
        <radialGradient id={`${id}-halo`}>
          <stop stopColor="#91abc8" stopOpacity=".06" />
          <stop offset="1" stopColor="#91abc8" stopOpacity="0" />
        </radialGradient>
      </defs>
      <ellipse cx={scene.halo.cx} cy={scene.halo.cy} rx={scene.halo.rx} ry={scene.halo.ry} fill={`url(#${id}-halo)`} />
      {scene.headings.map((h) => (
        <text key={h.text} className="up-heading" x={h.x} y={h.y} textAnchor={h.anchor}>
          {h.text}
        </text>
      ))}
      {scene.builds.map((b) => (
        <CableView key={b.key} cable={b.cable} dim={focus !== null && focus !== b.key} />
      ))}
      {scene.runners.map((r) => (
        <CableView key={r.key} cable={r.cable} dim={focus !== null && focus !== r.key} />
      ))}
      {flows.map((l, i) => (
        <g key={l.key} className="up-light" data-kind={l.kind} data-dim={dim(l.key)} style={{ "--up-delay": `${-((i * 1.3) % 3.6).toFixed(2)}s` } as React.CSSProperties} aria-hidden="true">
          <path d={l.cable.d} pathLength={100} className="up-light-halo" />
          <path d={l.cable.d} pathLength={100} className="up-light-core" />
        </g>
      ))}
      {pulses.map((r, i) => (
        <path
          key={r.key}
          d={r.cable.d}
          pathLength={100}
          className="up-pulse"
          data-dim={dim(r.key)}
          style={{ "--up-period": `${(6.4 + ((i * 0.55) % 1.6)).toFixed(2)}s`, "--up-delay": `${-(0.8 + i * 2.35).toFixed(2)}s` } as React.CSSProperties}
          aria-hidden="true"
        />
      ))}
      {scene.builds.map((b) => (
        <BuildView key={b.key} node={b} picked={picked === b.key} prism={prism} id={id} />
      ))}
      <ServerView node={scene.server} picked={picked === "server"} prism={prism} id={id} big={!scene.portrait} />
      {scene.runners.map((r) => (
        <RunnerView key={r.key} node={r} picked={picked === r.key} prism={prism} id={id} />
      ))}
    </svg>
  );
}

function CableView({ cable, dim }: { cable: Cable; dim: boolean }) {
  return (
    <g className="up-cable" data-tone={cable.tone} data-dim={dim || undefined}>
      <path d={cable.d} className="up-cable-shadow" />
      <path d={cable.d} className="up-cable-line" />
      {cable.arrow && <path d={cable.arrow} className="up-chevron" />}
    </g>
  );
}

/** A box as the figure draws one: its shadow, its glass walls, its etched lines, its cap and lit front edges. */
function Box({ box, id, prism }: { box: IsoBox; id: string; prism: string }) {
  return (
    <>
      {box.shadow && <ellipse className="up-shadow" cx={box.shadow.cx} cy={box.shadow.cy} rx={box.shadow.rx} ry={box.shadow.ry} />}
      <path d={box.left} fill={`url(#${id}-wall-l)`} stroke={prism} className="up-facet" />
      <path d={box.right} fill={`url(#${id}-wall-r)`} stroke={prism} className="up-facet" />
      {box.etches.map((d) => (
        <path key={d} d={d} className="up-etch" />
      ))}
      <path d={box.top} fill={`url(#${id}-cap)`} stroke={prism} className="up-facet" strokeOpacity={0.75} />
      <path d={box.ridge} stroke={prism} className="up-ridge" />
    </>
  );
}

/** A machine or a build to pick: the whole of it answers, by pointer or keyboard. */
function Node({ nodeKey, title, picked, hit, children }: { nodeKey: string; title: string; picked: boolean; hit: { x: number; y: number; w: number; h: number }; children: React.ReactNode }) {
  return (
    <g
      className="up-node"
      role="button"
      tabIndex={0}
      aria-pressed={picked}
      aria-label={title}
      data-picked={picked || undefined}
      onClick={() => pick(nodeKey)}
      onKeyDown={(e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          pick(nodeKey);
        }
      }}
    >
      <rect className="up-hit" x={hit.x} y={hit.y} width={hit.w} height={hit.h} rx={5} />
      {children}
    </g>
  );
}

function BuildView({ node, picked, prism, id }: { node: BuildNode; picked: boolean; prism: string; id: string }) {
  return (
    <Node nodeKey={node.key} title={node.title} picked={picked} hit={node.hit}>
      <Box box={node.chip} id={id} prism={prism} />
      <path d={node.die} pathLength={42} stroke={prism} className="up-die" data-building={node.building || undefined} />
      <text className="up-ink" x={node.name.x} y={node.name.y}>
        {node.name.text}
        {node.name.tail && (
          <tspan className="up-tail" dx={7}>
            {node.name.tail}
          </tspan>
        )}
      </text>
      <text className="up-sub" data-tone={attention(node.tone)} x={node.sub.x} y={node.sub.y}>
        {node.sub.text}
      </text>
    </Node>
  );
}

/** A line's tone, said only where it needs attention: all's well reads plain. */
const attention = (tone: Tone) => (tone === "ok" ? undefined : tone);

function ServerView({ node, picked, prism, id, big }: { node: ServerNode; picked: boolean; prism: string; id: string; big: boolean }) {
  return (
    <Node nodeKey={node.key} title={node.title} picked={picked} hit={node.hit}>
      <Box box={node.box} id={id} prism={prism} />
      <g transform={node.mark}>
        <path d={MARK.loop} stroke={prism} className="up-mark" />
        <path d={MARK.inner} stroke={prism} className="up-mark" strokeOpacity={0.28} />
        <path d={MARK.tail} className="up-mark-tail" />
      </g>
      <circle cx={node.led.x} cy={node.led.y} r={node.led.r} fill={LED[node.tone]} className="up-led" />
      <text className={big ? "up-node-name" : "up-node-name up-node-name-small"} x={node.name.x} y={node.name.y} textAnchor={node.name.anchor}>
        {node.name.text}
      </text>
      <text className="up-sub" data-tone={attention(node.tone)} x={node.sub.x} y={node.sub.y} textAnchor={node.sub.anchor}>
        {node.sub.text}
      </text>
    </Node>
  );
}

function RunnerView({ node, picked, prism, id }: { node: RunnerNode; picked: boolean; prism: string; id: string }) {
  return (
    <Node nodeKey={node.key} title={node.title} picked={picked} hit={node.hit}>
      <g className="up-unit" opacity={node.switching ? 0.55 : node.offline ? 0.5 : 1}>
        <Box box={node.box} id={id} prism={prism} />
        <circle cx={node.led.x} cy={node.led.y} r={node.led.r} fill={LED[node.tone]} className="up-led" />
      </g>
      {node.switching && (
        <g className="up-next">
          <Box box={node.next} id={id} prism={prism} />
        </g>
      )}
      <text className="up-ink" x={node.name.x} y={node.name.y}>
        {node.name.text}
      </text>
      <text className="up-sub" data-tone={attention(node.tone)} x={node.sub.x} y={node.sub.y}>
        {node.sub.text}
      </text>
    </Node>
  );
}
