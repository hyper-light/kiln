import { createFileRoute } from "@tanstack/react-router";
import MetricsExplorer from "@/views/observe/metrics";

export const Route = createFileRoute("/_app/observe/metrics")({ component: MetricsExplorer });
