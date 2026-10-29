import { createFileRoute } from "@tanstack/react-router";
import { LogsExplorer } from "@/views/observe/logs";

function Events() {
  return <LogsExplorer kind="events" />;
}

export const Route = createFileRoute("/_app/observe/events")({ component: Events });
