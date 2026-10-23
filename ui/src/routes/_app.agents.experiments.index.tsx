import { createFileRoute } from "@tanstack/react-router";
import ExperimentsPage from "@/views/experiments/index";

export const Route = createFileRoute("/_app/agents/experiments/")({ component: ExperimentsPage });
