import { createFileRoute } from "@tanstack/react-router";
import NewDeployApp from "@/views/releases/new";

export const Route = createFileRoute("/_app/deployments/releases/new")({ component: NewDeployApp });
