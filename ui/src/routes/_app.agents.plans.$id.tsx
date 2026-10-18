import { createFileRoute } from "@tanstack/react-router";
import PlanPage from "@/views/plans/plan";

export const Route = createFileRoute("/_app/agents/plans/$id")({ component: PlanPage });
