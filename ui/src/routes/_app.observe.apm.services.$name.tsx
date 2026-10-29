import { createFileRoute } from "@tanstack/react-router";
import { ApmService } from "@/views/observe/apm";

export const Route = createFileRoute("/_app/observe/apm/services/$name")({ component: ApmService });
