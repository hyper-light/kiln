import * as React from "react";
import type { ChartProps } from "./types";
import { Hold } from "@/components/ui/hold";

// uPlot lives in its own chunk; until it arrives, its space is held.
const UPlotChart = React.lazy(() => import("./uplot-chart"));

export function Chart(props: ChartProps) {
  const h = props.height ?? 200;
  return (
    <React.Suspense fallback={<Hold height={h + (props.legend === false ? 0 : 26)} />}>
      <UPlotChart {...props} />
    </React.Suspense>
  );
}
