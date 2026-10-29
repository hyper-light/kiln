import { createFileRoute } from "@tanstack/react-router";
import ShipPage from "@/views/ship";

export const Route = createFileRoute("/_app/ship")({ component: ShipPage });
