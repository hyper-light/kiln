import { createFileRoute } from "@tanstack/react-router";
import SloPage from "@/views/observe/slo";

export const Route = createFileRoute("/_app/observe/slos/$id")({ component: SloPage });
