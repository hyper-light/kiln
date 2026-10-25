import { createFileRoute } from "@tanstack/react-router";
import { FleetPage } from "@/views/releases/fleets";

export const Route = createFileRoute("/_app/deployments/releases/fleets/$id")({ component: FleetPage });
