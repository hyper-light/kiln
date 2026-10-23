import { createFileRoute } from "@tanstack/react-router";
import ExperimentPage from "@/views/experiments/experiment";

export const Route = createFileRoute("/_app/agents/experiments/$id")({ component: ExperimentPage });
