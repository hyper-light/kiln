import { createFileRoute } from "@tanstack/react-router";
import MetricsSummary from "@/views/observe/metricsummary";

export const Route = createFileRoute("/_app/observe/metrics-summary")({ component: MetricsSummary });
