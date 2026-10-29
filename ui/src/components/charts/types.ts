export interface SeriesSpec {
  label: string;
  color: string;
  /** true = 8% tint of color, string = explicit fill */
  fill?: boolean | string;
  bars?: boolean;
  scale?: "y" | "y2";
  width?: number;
  dash?: number[];
  /** Show its points where they're sparse (8px, ringed by the surface), so a lone value between gaps reads. */
  points?: boolean;
  /** Its samples' usual spacing, in the grid's buckets: a run of empty buckets no wider than twice it
   * (a sample missed) is drawn across; a wider one is a gap. A metric sent every minute on a grid of
   * 15 s is four; one sent every grid step, one. */
  spacing?: number;
  /** custom legend value: receives the raw data arrays */
  value?: (data: ArrayLike<ArrayLike<number | null | undefined>>, v: number | null, seriesIdx: number, idx: number) => string;
}

/** A band about a series (an anomaly function's): drawn beneath the lines in its series' color, its
 * points past it marked. */
export interface Band {
  /** The series (its place in `series`) it is drawn about. */
  series: number;
  lower: (number | null)[];
  upper: (number | null)[];
  /** The points past it. */
  anomalous?: boolean[];
}

export interface ChartProps {
  /** [xs (unix seconds), ...ys] */
  data: (number | null)[][];
  series: SeriesSpec[];
  height?: number;
  yFormat?: (v: number) => string;
  y2Format?: (v: number) => string;
  yRange?: [number, number];
  timeFormat?: "time" | "hour" | "day";
  /** The legend's reading of the x under the cursor (unix seconds); the time of day unless said. */
  xValue?: (ts: number) => string;
  legend?: boolean;
  className?: string;
  ariaLabel?: string;
  bands?: Band[];
}

/** Palette for series, harmonised with the prism stops and status pastels. */
export const SERIES = {
  ok: "#a8c9bc",
  fail: "#d1aaad",
  warn: "#cbb69a",
  pending: "#a6b2c6",
  violet: "#b8a6d5",
  blue: "#a6c4ed",
  teal: "#b8d9d2",
  rose: "#d6a2aa",
  sand: "#d8cbb0",
  grey: "#8d9097",
};
