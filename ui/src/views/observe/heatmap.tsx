/**
 * Heatmaps and distributions (docs/observe.md §4.4, §11): a query's values counted, in each bucket
 * of time, by the row they fall in (a distribution's rows its buckets, any other query's equal
 * ranges of its series' values). The heatmap draws each cell in one hue, brighter for more, its
 * count on a log scale so a few busy cells do not wash the rest out; the distribution draws the
 * whole range's counts as bars, its median and 99th percentile (the server's) marked.
 */
import * as React from "react";
import type { HeatmapAnswer } from "@/api/observe";
import { SERIES } from "@/components/charts/types";
import { formatValue } from "./parts";

/** The palette's first hue: magnitude in one hue, its lightness the count. */
const HUE = SERIES.violet;
const AXIS = "#7f838d";
const FONT = '10px "Geist Mono Variable", ui-monospace, monospace';
/** Room for the rows' labels, and the times' or the values'. */
const LEFT = 64;
const BOTTOM = 18;

function rgba(hex: string, a: number) {
  const n = parseInt(hex.replace("#", "").slice(0, 6), 16);
  return `rgba(${(n >> 16) & 255},${(n >> 8) & 255},${n & 255},${a})`;
}

/** A cell's opacity over the surface: none for no values; past that, its count's log over the most's. */
const shade = (c: number, max: number) => (c <= 0 || max <= 0 ? 0 : 0.14 + 0.86 * (Math.log1p(c) / Math.log1p(max)));

/** A row's value as a label: its lower bound (zero's own row, zero). */
const rowLabel = (row: [number, number], unit?: string) => formatValue(row[0], unit);

const timeLabel = (ms: number, day: boolean) => {
  const d = new Date(ms);
  return day ? d.toLocaleDateString(undefined, { month: "short", day: "numeric" }) : `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
};

/**
 * A canvas drawn by `paint` at its own size (redrawn as it resizes, and when `paint` changes): a
 * ref, as the timeseries chart's, not an effect.
 */
export function useCanvas(paint: (ctx: CanvasRenderingContext2D, w: number, h: number) => void, height: number) {
  return React.useCallback(
    (el: HTMLCanvasElement | null) => {
      if (!el) return;
      const draw = () => {
        const dpr = window.devicePixelRatio || 1;
        const w = Math.max(120, el.clientWidth);
        el.width = Math.round(w * dpr);
        el.height = Math.round(height * dpr);
        const ctx = el.getContext("2d");
        if (!ctx) return;
        ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
        ctx.clearRect(0, 0, w, height);
        paint(ctx, w, height);
      };
      draw();
      let raf = 0;
      const ro = new ResizeObserver(() => {
        cancelAnimationFrame(raf);
        raf = requestAnimationFrame(draw);
      });
      ro.observe(el);
      return () => {
        cancelAnimationFrame(raf);
        ro.disconnect();
      };
    },
    [paint, height],
  );
}

/** Up to `n` of `count` places, evenly spread, the last included. */
const spread = (count: number, n: number) => (count <= n ? Array.from({ length: count }, (_, i) => i) : Array.from({ length: n }, (_, i) => Math.round((i * (count - 1)) / (n - 1))));

export function HeatmapChart({ h, height, unit, day }: { h: HeatmapAnswer; height: number; unit?: string; day: boolean }) {
  const { rows, counts } = h.heatmap;
  const [hover, setHover] = React.useState<{ t: number; r: number; x: number; y: number } | null>(null);
  const max = counts.reduce((m, row) => row.reduce((a, c) => Math.max(a, c), m), 0);
  const times = React.useMemo(() => Array.from({ length: h.grid.n }, (_, i) => h.grid.times?.[i] ?? h.grid.from + i * h.grid.step), [h]);
  const plotH = height - BOTTOM;
  const paint = React.useCallback(
    (ctx: CanvasRenderingContext2D, w: number) => {
      const plotW = w - LEFT;
      const cw = plotW / Math.max(1, counts.length);
      const ch = plotH / Math.max(1, rows.length);
      // A surface gap between cells where they are big enough to keep one.
      const gap = cw >= 6 && ch >= 6 ? 1 : 0;
      for (let t = 0; t < counts.length; t++) {
        for (let r = 0; r < rows.length; r++) {
          const a = shade(counts[t]?.[r] ?? 0, max);
          if (a === 0) continue;
          ctx.fillStyle = rgba(HUE, a);
          ctx.fillRect(LEFT + t * cw, plotH - (r + 1) * ch, Math.max(1, cw - gap), Math.max(1, ch - gap));
        }
      }
      ctx.fillStyle = AXIS;
      ctx.font = FONT;
      ctx.textBaseline = "middle";
      ctx.textAlign = "right";
      for (const r of spread(rows.length, Math.max(2, Math.floor(plotH / 28)))) {
        const row = rows[r];
        // Kept inside the canvas: the top row's label below its edge.
        if (row) ctx.fillText(rowLabel(row, unit), LEFT - 6, Math.min(plotH - 4, Math.max(7, plotH - (r + 0.5) * ch)));
      }
      ctx.textBaseline = "alphabetic";
      ctx.textAlign = "center";
      for (const t of spread(times.length, Math.max(2, Math.floor(plotW / 90)))) {
        const at = times[t];
        if (at !== undefined) ctx.fillText(timeLabel(at, day), Math.min(w - 24, Math.max(LEFT + 24, LEFT + (t + 0.5) * cw)), height - 4);
      }
    },
    [counts, rows, max, times, plotH, height, unit, day],
  );
  const ref = useCanvas(paint, height);
  if (rows.length === 0)
    return (
      <p className="empty-line flex items-center justify-center" style={{ minHeight: height }}>
        No values in this range.
      </p>
    );
  const move = (e: React.PointerEvent<HTMLCanvasElement>) => {
    const box = e.currentTarget.getBoundingClientRect();
    const x = e.clientX - box.left;
    const y = e.clientY - box.top;
    const t = Math.floor(((x - LEFT) / (box.width - LEFT)) * counts.length);
    const r = rows.length - 1 - Math.floor((y / plotH) * rows.length);
    setHover(t >= 0 && t < counts.length && r >= 0 && r < rows.length && y < plotH ? { t, r, x, y } : null);
  };
  const row = hover ? rows[hover.r] : undefined;
  const at = hover ? times[hover.t] : undefined;
  return (
    <div className="relative">
      <canvas
        ref={ref}
        style={{ width: "100%", height }}
        onPointerMove={move}
        onPointerDown={move}
        onPointerLeave={(e) => e.pointerType === "mouse" && setHover(null)}
        role="img"
        aria-label={`A heatmap: ${rows.length} rows of values over ${counts.length} buckets of time, ${max.toLocaleString()} in the busiest cell`}
      />
      {hover && row && at !== undefined && (
        <div
          className="obs-tooltip pointer-events-none absolute z-10 rounded border border-[#2a2d34] bg-[#121317] px-2 py-1 font-mono text-[11px] text-[#e4e5e7] shadow"
          style={{ left: Math.min(hover.x + 12, 9999), top: Math.max(0, hover.y - 36) }}
        >
          <div className="text-[#8c8f98]">{new Date(at).toLocaleString()}</div>
          <div>
            {formatValue(row[0], unit)} – {formatValue(row[1], unit)}: {(counts[hover.t]?.[hover.r] ?? 0).toLocaleString()}
          </div>
        </div>
      )}
      <Scale max={max} />
    </div>
  );
}

/** What the shades mean: none, one, and the busiest cell's count. */
function Scale({ max }: { max: number }) {
  return (
    <div className="mt-1 flex items-center justify-end gap-2 font-mono text-[10px] text-[#7f838d]" aria-hidden="true">
      <span>1</span>
      <span className="h-2 w-24 rounded-sm" style={{ background: `linear-gradient(to right, ${rgba(HUE, 0.14)}, ${rgba(HUE, 1)})` }} />
      <span>{max.toLocaleString()}</span>
    </div>
  );
}

/** The row whose range holds `v` (the last at or below it). */
function rowOf(rows: [number, number][], v: number): number {
  let at = 0;
  for (let r = 0; r < rows.length; r++) {
    const row = rows[r];
    if (row && row[0] <= v) at = r;
  }
  return at;
}

/**
 * `h`'s rows' counts over the whole range, as bars, with `percentiles` marked where they fall: the
 * server's own (`p50:`, `p99:` of the same query), not read again from the rows, which may be
 * coarser than the buckets they came of.
 */
export function DistributionChart({ h, height, unit, percentiles }: { h: HeatmapAnswer; height: number; unit?: string; percentiles: { label: string; v: number | null }[] }) {
  const { rows, counts } = h.heatmap;
  const [hover, setHover] = React.useState<{ r: number; x: number; y: number } | null>(null);
  const totals = React.useMemo(() => rows.map((_, r) => counts.reduce((a, row) => a + (row[r] ?? 0), 0)), [rows, counts]);
  const max = Math.max(0, ...totals);
  const marks = React.useMemo(() => percentiles.map((p) => [p.label, p.v === null || !Number.isFinite(p.v) ? null : { r: rowOf(rows, p.v), v: p.v }] as const), [percentiles, rows]);
  const plotH = height - BOTTOM;
  const paint = React.useCallback(
    (ctx: CanvasRenderingContext2D, w: number) => {
      const plotW = w - LEFT;
      const bw = plotW / Math.max(1, rows.length);
      // Bars a 2 px surface gap apart where they are wide enough; their tops rounded.
      const gap = bw >= 6 ? 2 : 0;
      ctx.fillStyle = HUE;
      for (let r = 0; r < rows.length; r++) {
        const v = totals[r] ?? 0;
        if (v <= 0) continue;
        const bh = Math.max(1, (v / max) * (plotH - 14));
        const x = LEFT + r * bw;
        ctx.beginPath();
        ctx.roundRect(x, plotH - bh, Math.max(1, bw - gap), bh, [Math.min(4, bw / 2), Math.min(4, bw / 2), 0, 0]);
        ctx.fill();
      }
      ctx.font = FONT;
      // Each mark's label on its line's top, a line lower where it would meet the one before.
      let taken: [number, number] | null = null;
      for (const [label, m] of marks) {
        if (!m) continue;
        const x = LEFT + (m.r + 0.5) * bw;
        const text = `${label} ${formatValue(m.v, unit)}`;
        const width = ctx.measureText(text).width;
        const right = x > w - width - 12;
        const span: [number, number] = right ? [x - 4 - width, x - 4] : [x + 4, x + 4 + width];
        const lower = taken !== null && span[0] < taken[1] + 6 && taken[0] < span[1] + 6;
        const y = lower ? 22 : 10;
        ctx.strokeStyle = "#e4e5e7";
        ctx.lineWidth = 1;
        ctx.setLineDash([3, 3]);
        ctx.beginPath();
        ctx.moveTo(x, y + 2);
        ctx.lineTo(x, plotH);
        ctx.stroke();
        ctx.setLineDash([]);
        ctx.fillStyle = "#c8cad0";
        ctx.textAlign = right ? "right" : "left";
        ctx.fillText(text, right ? x - 4 : x + 4, y);
        taken = span;
      }
      ctx.fillStyle = AXIS;
      ctx.textAlign = "center";
      for (const r of spread(rows.length, Math.max(2, Math.floor(plotW / 80)))) {
        const row = rows[r];
        if (row) ctx.fillText(rowLabel(row, unit), Math.min(w - 24, Math.max(LEFT + 24, LEFT + (r + 0.5) * bw)), height - 4);
      }
      ctx.textAlign = "right";
      ctx.textBaseline = "middle";
      ctx.fillText(max.toLocaleString(), LEFT - 6, 14);
      ctx.fillText("0", LEFT - 6, plotH);
      ctx.textBaseline = "alphabetic";
    },
    [rows, totals, max, marks, plotH, height, unit],
  );
  const ref = useCanvas(paint, height);
  if (rows.length === 0)
    return (
      <p className="empty-line flex items-center justify-center" style={{ minHeight: height }}>
        No values in this range.
      </p>
    );
  const move = (e: React.PointerEvent<HTMLCanvasElement>) => {
    const box = e.currentTarget.getBoundingClientRect();
    const x = e.clientX - box.left;
    const r = Math.floor(((x - LEFT) / (box.width - LEFT)) * rows.length);
    setHover(r >= 0 && r < rows.length ? { r, x, y: e.clientY - box.top } : null);
  };
  const row = hover ? rows[hover.r] : undefined;
  const said = marks.flatMap(([label, m]) => (m ? [`${label} ${formatValue(m.v, unit)}`] : []));
  return (
    <div className="relative">
      <canvas
        ref={ref}
        style={{ width: "100%", height }}
        onPointerMove={move}
        onPointerDown={move}
        onPointerLeave={(e) => e.pointerType === "mouse" && setHover(null)}
        role="img"
        aria-label={`A distribution of ${totals.reduce((a, b) => a + b, 0).toLocaleString()} values${said.length ? `, ${said.join(", ")}` : ""}`}
      />
      {hover && row && (
        <div
          className="obs-tooltip pointer-events-none absolute z-10 rounded border border-[#2a2d34] bg-[#121317] px-2 py-1 font-mono text-[11px] text-[#e4e5e7] shadow"
          style={{ left: hover.x + 12, top: Math.max(0, hover.y - 30) }}
        >
          {formatValue(row[0], unit)} – {formatValue(row[1], unit)}: {(totals[hover.r] ?? 0).toLocaleString()}
        </div>
      )}
    </div>
  );
}
