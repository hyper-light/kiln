/**
 * A figure's stage: an SVG drawn from a scene function, one SVG unit to a CSS pixel, repainted on a
 * clock by writing only the attributes that changed. React renders the topology (which paths and
 * labels exist); the clock moves them.
 */
import * as React from "react";
import { cn } from "@/lib/utils";
import { toneColor, type DrawFn, type FigLabel, type FigPath, type Frame } from "./geometry";
import { useClock, useMotion } from "./motion";

type Painter = (f: Frame, t: number) => void;

function painter(svg: SVGSVGElement, w: number, h: number): Painter {
  const paths = Array.from(svg.querySelectorAll<SVGPathElement>("[data-fig-path]"));
  const labels = Array.from(svg.querySelectorAll<SVGTextElement>("[data-fig-label]"));
  const hits = Array.from(svg.querySelectorAll<SVGCircleElement>("[data-fig-hit]"));
  const prism = svg.querySelector<SVGLinearGradientElement>("[data-fig-prism]");
  const prevPaths: (FigPath | undefined)[] = [];
  const prevLabels: (FigLabel | undefined)[] = [];
  let prismTick = -1;
  return (f, t) => {
    for (let i = 0; i < f.paths.length; i++) {
      const p = f.paths[i]!;
      const el = paths[i];
      if (!el) break;
      const o = prevPaths[i];
      if (!o || o.d !== p.d) el.setAttribute("d", p.d);
      if (!o || o.opacity !== p.opacity) {
        el.setAttribute("opacity", p.opacity.toFixed(3));
        if (p.opacity === 0) el.setAttribute("display", "none");
        else el.removeAttribute("display");
      }
      if (!o || o.tone !== p.tone) el.style.stroke = toneColor(p.tone) ?? "";
      if (p.kind === "light") el.setAttribute("stroke-dashoffset", (-t * (p.speed ?? 22) + i * 7).toFixed(2));
      prevPaths[i] = p;
    }
    for (let i = 0; i < f.labels.length; i++) {
      const l = f.labels[i]!;
      const el = labels[i];
      if (!el) break;
      const o = prevLabels[i];
      if (!o || o.x !== l.x) el.setAttribute("x", l.x.toFixed(1));
      if (!o || o.y !== l.y) el.setAttribute("y", l.y.toFixed(1));
      if (!o || o.opacity !== l.opacity) {
        el.setAttribute("opacity", l.opacity.toFixed(3));
        if (l.opacity === 0) el.setAttribute("display", "none");
        else el.removeAttribute("display");
      }
      if (!o || o.tone !== l.tone) el.style.fill = toneColor(l.tone) ?? "";
      if (!o || o.text !== l.text) el.textContent = l.text;
      prevLabels[i] = l;
    }
    for (let i = 0; i < f.hits.length; i++) {
      const hEl = hits[i];
      const hit = f.hits[i]!;
      if (!hEl) break;
      hEl.setAttribute("cx", hit.x.toFixed(1));
      hEl.setAttribute("cy", hit.y.toFixed(1));
      hEl.setAttribute("r", hit.r.toFixed(1));
    }
    // The slow prism wash across every stroke; 10 Hz is plenty for it.
    const tick = Math.floor(t * 10);
    if (prism && tick !== prismTick) {
      prismTick = tick;
      const pt = tick / 10;
      prism.setAttribute("x1", (-0.12 * w + Math.sin(pt * 0.23) * 0.16 * w).toFixed(1));
      prism.setAttribute("x2", (0.95 * w + Math.sin(pt * 0.19) * 0.14 * w).toFixed(1));
      prism.setAttribute("y2", (0.7 * h).toFixed(1));
    }
  };
}

export interface SceneProps {
  draw: DrawFn;
  /** Stage height in pixels, or a function of its width. */
  height: number | ((w: number) => number);
  className?: string;
  /** Clicks on a moving target (an agent's craft): its id. */
  onHit?: (id: string) => void;
  fps?: number;
  /** Paint once and stand still regardless of the motion setting (thumbnails, previews). */
  still?: boolean;
}

export function Scene({ draw, height, className, onHit, fps = 30, still = false }: SceneProps) {
  const wrap = React.useRef<HTMLDivElement>(null);
  const svg = React.useRef<SVGSVGElement>(null);
  const [w, setW] = React.useState(0);
  React.useLayoutEffect(() => {
    const el = wrap.current;
    if (!el) return;
    setW(Math.round(el.getBoundingClientRect().width));
    const ro = new ResizeObserver((entries) => {
      const width = entries.at(-1)?.contentRect.width;
      if (width) setW(Math.round(width));
    });
    ro.observe(el);
    return () => ro.disconnect();
  }, []);
  const h = Math.round(typeof height === "function" ? height(w) : height);
  const { paused } = useMotion();
  const clock = React.useRef(0);
  const paint = React.useRef<Painter | null>(null);
  // Topology: which paths and labels exist, from the scene at the current time.
  const initial = React.useMemo(() => (w > 0 ? draw(clock.current, w, h) : null), [draw, w, h]);
  const id = React.useId().replace(/[^a-zA-Z0-9_-]/g, "");
  React.useLayoutEffect(() => {
    const el = svg.current;
    if (!el || !initial) return;
    paint.current = painter(el, w, h);
    paint.current(initial, clock.current);
  }, [initial, w, h]);
  const drawRef = React.useRef(draw);
  React.useLayoutEffect(() => {
    drawRef.current = draw;
  });
  useClock(
    svg,
    !paused && !still && w > 0,
    (t) => {
      clock.current = t;
      if (paint.current) paint.current(drawRef.current(t, w, h), t);
    },
    fps,
  );

  return (
    <div ref={wrap} className={cn("fig-stage", className)} style={{ height: h || undefined }}>
      {initial && (
        <svg ref={svg} className="fig-svg" viewBox={`0 0 ${w} ${h}`} width={w} height={h} fill="none" aria-hidden={onHit ? undefined : true} focusable="false">
          <defs>
            <linearGradient id={`${id}-prism`} data-fig-prism="" gradientUnits="userSpaceOnUse" x1={-0.12 * w} y1={0.15 * h} x2={0.95 * w} y2={0.7 * h}>
              <stop stopColor="#a4cabe" />
              <stop offset=".34" stopColor="#9db8d5" />
              <stop offset=".68" stopColor="#b7a2c9" />
              <stop offset="1" stopColor="#d0bda0" />
            </linearGradient>
            <linearGradient id={`${id}-glass`} x1="0" y1="0" x2=".8" y2="1">
              <stop stopColor="#b8cbd7" stopOpacity=".2" />
              <stop offset=".22" stopColor="#7c91a1" stopOpacity=".02" />
              <stop offset=".63" stopColor="#c0bdd5" stopOpacity=".1" />
              <stop offset="1" stopColor="#a4bacb" stopOpacity=".025" />
            </linearGradient>
            <radialGradient id={`${id}-halo`}>
              <stop stopColor="#94acc1" stopOpacity=".06" />
              <stop offset="1" stopColor="#94acc1" stopOpacity="0" />
            </radialGradient>
          </defs>
          <ellipse cx={w / 2} cy={h * 0.45} rx={w * 0.46} ry={h * 0.42} fill={`url(#${id}-halo)`} />
          {initial.paths.map((p) => (
            <path
              key={p.id}
              data-fig-path={p.id}
              className={`fig-${p.kind}`}
              d={p.d}
              opacity={p.opacity.toFixed(3)}
              display={p.opacity === 0 ? "none" : undefined}
              fill={p.kind === "glass" || p.kind === "shade" ? `url(#${id}-glass)` : "none"}
              stroke={`url(#${id}-prism)`}
              style={{ stroke: toneColor(p.tone) }}
              vectorEffect="non-scaling-stroke"
              pathLength={p.kind === "light" ? 200 : undefined}
              strokeDasharray={p.dash ?? (p.kind === "light" ? "12 188" : undefined)}
            />
          ))}
          {initial.labels.map((l) => (
            <text
              key={l.id}
              data-fig-label={l.id}
              className={`fig-t-${l.kind}`}
              x={l.x}
              y={l.y}
              textAnchor={l.anchor}
              opacity={l.opacity}
              display={l.opacity === 0 ? "none" : undefined}
              style={{ fill: toneColor(l.tone) }}
            >
              {l.text}
            </text>
          ))}
          {onHit &&
            initial.hits.map((hit) => (
              <circle key={hit.id} data-fig-hit={hit.id} className="fig-hit" cx={hit.x} cy={hit.y} r={hit.r} onClick={() => onHit(hit.id)}>
                <title>{hit.label}</title>
              </circle>
            ))}
        </svg>
      )}
    </div>
  );
}
