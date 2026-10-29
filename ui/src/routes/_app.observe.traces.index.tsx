import { createFileRoute } from "@tanstack/react-router";
import TraceExplorer from "@/views/observe/traces";

export const Route = createFileRoute("/_app/observe/traces/")({ component: TraceExplorer });
