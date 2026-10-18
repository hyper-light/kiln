import { createFileRoute } from "@tanstack/react-router";
import NewApp from "@/views/deployments/new";

export const Route = createFileRoute("/_app/deployments/new")({ component: NewApp });
