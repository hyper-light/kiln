import { createFileRoute } from "@tanstack/react-router";
import Incidents from "@/views/observe/incidents";

export const Route = createFileRoute("/_app/observe/incidents/")({ component: Incidents });
