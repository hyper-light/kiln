import { createFileRoute } from "@tanstack/react-router";
import IncidentPage from "@/views/observe/incident";

export const Route = createFileRoute("/_app/observe/incidents/$id")({ component: IncidentPage });
