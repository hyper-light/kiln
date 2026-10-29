import { createFileRoute } from "@tanstack/react-router";
import ServiceMapPage from "@/views/observe/servicemap";

export const Route = createFileRoute("/_app/observe/apm/map")({ component: ServiceMapPage });
