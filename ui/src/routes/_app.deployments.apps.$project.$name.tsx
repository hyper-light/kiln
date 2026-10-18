import { createFileRoute } from "@tanstack/react-router";
import DeploymentApp from "@/views/deployments/app";

export const Route = createFileRoute("/_app/deployments/apps/$project/$name")({ component: DeploymentApp });
