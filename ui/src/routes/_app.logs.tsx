import { createFileRoute } from "@tanstack/react-router";
import Logs from "@/views/ci/logs";

export const Route = createFileRoute("/_app/logs")({ component: Logs });
