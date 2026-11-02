import { createFileRoute } from "@tanstack/react-router";
import Dashboards from "@/views/observe/dashboards";

export const Route = createFileRoute("/_app/observe/dashboards/")({ component: Dashboards });
