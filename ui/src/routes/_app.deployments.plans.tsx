import { createFileRoute } from "@tanstack/react-router";
import DeployPlans from "@/views/deployments/plans";

export const Route = createFileRoute("/_app/deployments/plans")({ component: DeployPlans });
