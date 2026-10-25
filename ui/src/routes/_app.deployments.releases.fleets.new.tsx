import { createFileRoute } from "@tanstack/react-router";
import NewFleet from "@/views/releases/fleets";

export const Route = createFileRoute("/_app/deployments/releases/fleets/new")({ component: NewFleet });
