import { createFileRoute } from "@tanstack/react-router";
import LogMetrics from "@/views/observe/logmetrics";

export const Route = createFileRoute("/_app/observe/log-metrics")({ component: LogMetrics });
