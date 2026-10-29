import { createFileRoute } from "@tanstack/react-router";
import DashboardPage from "@/views/observe/dashboard";

export const Route = createFileRoute("/_app/observe/dashboards/$id")({ component: DashboardPage });
