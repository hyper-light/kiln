import * as React from "react";
import uPlot from "uplot";
import "uplot/dist/uPlot.min.css";
import type { Band, ChartProps } from "./types";

const AXIS_FONT = '10px "Geist Mono Variable", ui-monospace, monospace';

/**
 * A y axis as wide as its longest label, 52 px at the least ("47.7 KiB/s" whole, not cut to
 * ".7 KiB/s"): uPlot's own way, its labels measured in the axis's font as they are drawn.
 */
const fitLabels: uPlot.Axis.Size = (u, values, axisIdx, cycleNum) => {
  const axis = u.axes[axisIdx] as uPlot.Axis & { _size?: number; font?: string | [string, number]; gap?: number };
  // Settled on the first cycles: a width that changes the labels does not change it again.
  if (cycleNum > 1) return axis._size ?? 52;
  const longest = (values ?? []).reduce((a, v) => (v.length > a.length ? v : a), "");
  if (!longest) return 52;
  u.ctx.font = (Array.isArray(axis.font) ? axis.font[0] : axis.font) ?? AXIS_FONT;
  return Math.max(52, Math.ceil(u.ctx.measureText(longest).width / (window.devicePixelRatio || 1) + (axis.gap ?? 5) + 6));
};
const GRID = "#191c21";
const AXIS = "#7f838d";

function hexA(hex: string, a: number) {
  const h = hex.replace("#", "");
  const n = parseInt(h.length === 3 ? h.replace(/./g, "$&$&") : h.slice(0, 6), 16);
  return `rgba(${(n >> 16) & 255},${(n >> 8) & 255},${n & 255},${a})`;
}

function timeFmt(kind: ChartProps["timeFormat"]) {
  return (_u: uPlot, splits: number[]) =>
    splits.map((s) => {
      const d = new Date(s * 1000);
      const hh = String(d.getHours()).padStart(2, "0");
      const mm = String(d.getMinutes()).padStart(2, "0");
      if (kind === "hour") return `${hh}:00`;
      if (kind === "day") return d.toLocaleDateString(undefined, { month: "short", day: "numeric" });
      return `${hh}:${mm}`;
    });
}

/** Each band beneath the lines: a polygon along its upper bound and back along its lower, broken where
 * either has no value; clipped to the plot. */
function drawBands(u: uPlot, bands: Band[], series: ChartProps["series"]) {
  const xs = u.data[0];
  const ctx = u.ctx;
  ctx.save();
  ctx.beginPath();
  ctx.rect(u.bbox.left, u.bbox.top, u.bbox.width, u.bbox.height);
  ctx.clip();
  for (const b of bands) {
    ctx.fillStyle = hexA(series[b.series]?.color ?? "#8d9097", 0.14);
    let run: number[] = [];
    const flush = () => {
      if (run.length > 1) {
        ctx.beginPath();
        run.forEach((i, j) => {
          const x = u.valToPos(xs[i]!, "x", true);
          const y = u.valToPos(b.upper[i]!, "y", true);
          if (j === 0) ctx.moveTo(x, y);
          else ctx.lineTo(x, y);
        });
        for (let j = run.length - 1; j >= 0; j--) {
          const i = run[j]!;
          ctx.lineTo(u.valToPos(xs[i]!, "x", true), u.valToPos(b.lower[i]!, "y", true));
        }
        ctx.closePath();
        ctx.fill();
      }
      run = [];
    };
    for (let i = 0; i < xs.length; i++) {
      if (b.upper[i] != null && b.lower[i] != null) run.push(i);
      else flush();
    }
    flush();
  }
  ctx.restore();
}

/** Each band's points past it, over the lines: a dot in the failure color. */
function drawPast(u: uPlot, bands: Band[]) {
  const xs = u.data[0];
  const ctx = u.ctx;
  ctx.save();
  ctx.fillStyle = "#d1aaad";
  for (const b of bands) {
    const ys = u.data[b.series + 1];
    b.anomalous?.forEach((past, i) => {
      const y = ys?.[i];
      if (!past || y == null) return;
      ctx.beginPath();
      ctx.arc(u.valToPos(xs[i]!, "x", true), u.valToPos(y, "y", true), 3 * devicePixelRatio, 0, 2 * Math.PI);
      ctx.fill();
    });
  }
  ctx.restore();
}

export default function UPlotChart({ data, series, height = 200, yFormat = (v) => String(v), y2Format, yRange, timeFormat = "time", xValue, legend = true, className, ariaLabel, bands = [] }: ChartProps) {
  const plotRef = React.useRef<uPlot | null>(null);
  const dataRef = React.useRef(data);
  dataRef.current = data;
  const bandsRef = React.useRef(bands);
  bandsRef.current = bands;
  // Its series as they are now (their spacing changes with the data, not the plot's structure).
  const seriesRef = React.useRef(series);
  seriesRef.current = series;

  // A stable key for structural changes (series set, formatters by identity are ignored).
  const structKey = JSON.stringify([series.map((s) => [s.label, s.color, s.bars, s.scale, s.fill, s.dash, s.points]), height, yRange, timeFormat, legend, Boolean(y2Format), bands.length > 0]);

  // The plot lives as long as its element and its structure: made when they arrive, destroyed with them.
  const mount = React.useCallback(
    (el: HTMLDivElement | null) => {
      if (!el) return;
      const width = Math.max(120, el.clientWidth);
      const hasY2 = series.some((s) => s.scale === "y2");
      const barsPaths = uPlot.paths.bars ? uPlot.paths.bars({ size: [0.62, 18], gap: 1 }) : undefined;

      const opts: uPlot.Options = {
        width,
        height,
        padding: [10, hasY2 ? 4 : 10, 0, 0],
        cursor: {
          points: { size: 6, fill: (u, si) => (u.series[si]?.stroke as string) ?? "#fff", stroke: "#08090a", width: 1.5 },
          drag: { x: false, y: false },
        },
        legend: { show: legend, live: true },
        scales: {
          x: { time: true },
          // From 0 to the highest line or band.
          y: yRange
            ? { range: yRange }
            : {
                range: (_u, _min, max) => {
                  const top = bandsRef.current.reduce((m, b) => b.upper.reduce<number>((n, v) => (v != null && v > n ? v : n), m), max);
                  return [0, top > 0 ? top * 1.12 : 1];
                },
              },
          ...(hasY2 ? { y2: { range: (_u: uPlot, _min: number, max: number) => [0, max > 0 ? max * 1.12 : 1] as uPlot.Range.MinMax } } : {}),
        },
        axes: [
          {
            stroke: AXIS,
            font: AXIS_FONT,
            grid: { show: false },
            ticks: { show: true, stroke: GRID, width: 1, size: 4 },
            values: timeFmt(timeFormat),
            space: 64,
          },
          {
            stroke: AXIS,
            font: AXIS_FONT,
            grid: { stroke: GRID, width: 1 },
            ticks: { show: false },
            size: fitLabels,
            values: (_u, splits) => splits.map((v) => yFormat(v)),
            space: 32,
          },
          ...(hasY2
            ? [
                {
                  scale: "y2",
                  side: 1 as const,
                  stroke: AXIS,
                  font: AXIS_FONT,
                  grid: { show: false },
                  ticks: { show: false },
                  size: fitLabels,
                  values: (_u: uPlot, splits: number[]) => splits.map((v) => (y2Format ?? yFormat)(v)),
                },
              ]
            : []),
        ],
        series: [
          { value: (_u, v) => (v == null ? "—" : xValue ? xValue(v) : new Date(v * 1000).toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit", hour12: false })) },
          ...series.map<uPlot.Series>((s) => ({
            label: s.label,
            scale: s.scale ?? "y",
            stroke: s.color,
            width: s.bars ? 0 : (s.width ?? 1.25),
            dash: s.dash,
            fill: s.bars ? hexA(s.color, 0.78) : s.fill ? (typeof s.fill === "string" ? s.fill : hexA(s.color, 0.08)) : undefined,
            paths: s.bars ? barsPaths : undefined,
            // uPlot shows them where there's room between them.
            points: s.points ? { size: 8, width: 2, stroke: "#08090a", fill: s.color } : { show: false },
            spanGaps: false,
            // A run of empty buckets no wider than twice its spacing (a sample missed) drawn across.
            gaps: (u: uPlot, si: number, _i0: number, _i1: number, nullGaps: uPlot.Series.Gaps) => {
              const spacing = seriesRef.current[si - 1]?.spacing ?? 1;
              const xs = u.data[0];
              const a = xs[0];
              const b = xs[1];
              if (a === undefined || b === undefined) return nullGaps;
              const step = Math.abs(u.valToPos(b, "x", true) - u.valToPos(a, "x", true));
              return nullGaps.filter(([from, to]) => to - from > step * (2 * spacing + 0.5));
            },
            value: s.value ? (u, v, si, i) => s.value!(u.data, v, si, i ?? 0) : (_u, v) => (v == null ? "—" : s.scale === "y2" && y2Format ? y2Format(v) : yFormat(v)),
          })),
        ],
      };

      opts.hooks = {
        drawAxes: [(u) => drawBands(u, bandsRef.current, series)],
        draw: [(u) => drawPast(u, bandsRef.current)],
      };
      const plot = new uPlot(opts, dataRef.current as uPlot.AlignedData, el);
      plotRef.current = plot;

      let raf = 0;
      const ro = new ResizeObserver(() => {
        cancelAnimationFrame(raf);
        raf = requestAnimationFrame(() => {
          const w = Math.max(120, el.clientWidth);
          if (Math.abs(w - plot.width) > 1) plot.setSize({ width: w, height });
        });
      });
      ro.observe(el);
      return () => {
        cancelAnimationFrame(raf);
        ro.disconnect();
        plot.destroy();
        plotRef.current = null;
      };
    },
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [structKey],
  );

  // New data goes into the plot there is (a ref that changes with the data, attached after it).
  const feed = React.useCallback(
    (el: HTMLSpanElement | null) => {
      if (el) plotRef.current?.setData(data as uPlot.AlignedData);
    },
    [data],
  );

  return (
    <>
      <div ref={mount} className={className} role="img" aria-label={ariaLabel} style={{ minHeight: height }} />
      <span ref={feed} hidden />
    </>
  );
}
