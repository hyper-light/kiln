import * as React from "react";
import uPlot from "uplot";
import "uplot/dist/uPlot.min.css";
import type { ChartProps } from "./types";

const AXIS_FONT = '10px "Geist Mono Variable", ui-monospace, monospace';
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

export default function UPlotChart({
  data,
  series,
  height = 200,
  yFormat = (v) => String(v),
  y2Format,
  yRange,
  timeFormat = "time",
  legend = true,
  className,
  ariaLabel,
}: ChartProps) {
  const wrapRef = React.useRef<HTMLDivElement>(null);
  const plotRef = React.useRef<uPlot | null>(null);
  const dataRef = React.useRef(data);
  dataRef.current = data;

  // A stable key for structural changes (series set, formatters by identity are ignored).
  const structKey = JSON.stringify([series.map((s) => [s.label, s.color, s.bars, s.scale, s.fill, s.dash]), height, yRange, timeFormat, legend, Boolean(y2Format)]);

  React.useEffect(() => {
    const el = wrapRef.current;
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
        y: yRange ? { range: yRange } : { range: (_u, _min, max) => [0, max > 0 ? max * 1.12 : 1] },
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
          size: 52,
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
                size: 52,
                values: (_u: uPlot, splits: number[]) => splits.map((v) => (y2Format ?? yFormat)(v)),
              },
            ]
          : []),
      ],
      series: [
        { value: (_u, v) => (v == null ? "—" : new Date(v * 1000).toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit", hour12: false })) },
        ...series.map<uPlot.Series>((s) => ({
          label: s.label,
          scale: s.scale ?? "y",
          stroke: s.color,
          width: s.bars ? 0 : (s.width ?? 1.25),
          dash: s.dash,
          fill: s.bars ? hexA(s.color, 0.78) : s.fill ? (typeof s.fill === "string" ? s.fill : hexA(s.color, 0.08)) : undefined,
          paths: s.bars ? barsPaths : undefined,
          points: { show: false },
          spanGaps: false,
          value: s.value
            ? (u, v, si, i) => s.value!(u.data, v, si, i ?? 0)
            : (_u, v) => (v == null ? "—" : s.scale === "y2" && y2Format ? y2Format(v) : yFormat(v)),
        })),
      ],
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
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [structKey]);

  React.useEffect(() => {
    plotRef.current?.setData(data as uPlot.AlignedData);
  }, [data]);

  return <div ref={wrapRef} className={className} role="img" aria-label={ariaLabel} style={{ minHeight: height }} />;
}
