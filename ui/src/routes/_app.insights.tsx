import { createFileRoute } from "@tanstack/react-router";
import Insights from "@/views/ci/insights";

export const Route = createFileRoute("/_app/insights")({ component: Insights });
