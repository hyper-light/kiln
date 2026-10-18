import { createFileRoute } from "@tanstack/react-router";
import Compare from "@/views/run-detail/compare";

export const Route = createFileRoute("/_app/runs/$id/compare")({ component: Compare });
