import { createFileRoute } from "@tanstack/react-router";
import Logs from "@/views/observe/logs";

export const Route = createFileRoute("/_app/observe/logs")({ component: Logs });
