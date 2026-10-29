import { createFileRoute } from "@tanstack/react-router";
import MonitorPage from "@/views/observe/monitor";

export const Route = createFileRoute("/_app/observe/monitors/$id")({ component: MonitorPage });
