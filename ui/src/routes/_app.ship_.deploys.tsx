import { createFileRoute } from "@tanstack/react-router";
import ShipDeploysPage from "@/views/ship/deploys";

export const Route = createFileRoute("/_app/ship_/deploys")({ component: ShipDeploysPage });
