import { createFileRoute } from "@tanstack/react-router";
import Downtimes from "@/views/observe/downtimes";

export const Route = createFileRoute("/_app/observe/downtimes")({ component: Downtimes });
