import { createFileRoute } from "@tanstack/react-router";
import ApmServices from "@/views/observe/apm";

export const Route = createFileRoute("/_app/observe/apm/")({ component: ApmServices });
