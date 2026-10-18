import { createFileRoute } from "@tanstack/react-router";
import Metrics from "@/views/metrics";

export const Route = createFileRoute("/_app/telemetry")({ component: Metrics });
