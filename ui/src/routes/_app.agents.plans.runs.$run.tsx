import { createFileRoute } from "@tanstack/react-router";
import RunPage from "@/views/plans/run";

export const Route = createFileRoute("/_app/agents/plans/runs/$run")({ component: RunPage });
