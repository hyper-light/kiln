import * as React from "react";
import type { ChartProps } from "./types";
import { Skeleton } from "@/components/ui/misc";

// uPlot lives in its own chunk; pages render a skeleton until it arrives.
const UPlotChart = React.lazy(() => import("./uplot-chart"));

export function Chart(props: ChartProps) {
  const h = props.height ?? 200;
  return (
    <React.Suspense fallback={<Skeleton className="w-full" style={{ height: h + (props.legend === false ? 0 : 26) }} />}>
      <UPlotChart {...props} />
    </React.Suspense>
  );
}
