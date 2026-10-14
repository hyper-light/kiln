export interface SeriesSpec {
  label: string;
  color: string;
  /** true = 8% tint of color, string = explicit fill */
  fill?: boolean | string;
  bars?: boolean;
  scale?: "y" | "y2";
  width?: number;
  dash?: number[];
  /** custom legend value: receives the raw data arrays */
  value?: (data: ArrayLike<ArrayLike<number | null | undefined>>, v: number | null, seriesIdx: number, idx: number) => string;
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
  legend?: boolean;
  className?: string;
  ariaLabel?: string;
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
