import { createFileRoute } from "@tanstack/react-router";
import ShipPipelinesPage from "@/views/ship/pipelines";

export const Route = createFileRoute("/_app/ship_/pipelines")({ component: ShipPipelinesPage });
