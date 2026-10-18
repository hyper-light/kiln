import { createFileRoute } from "@tanstack/react-router";
import RunDetail from "@/views/run-detail/index";

export const Route = createFileRoute("/_app/runs/$id/")({ component: RunDetail });
