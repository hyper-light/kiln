import { createFileRoute } from "@tanstack/react-router";
import Templates from "@/views/releases/templates";

export const Route = createFileRoute("/_app/deployments/releases/templates/")({ component: Templates });
