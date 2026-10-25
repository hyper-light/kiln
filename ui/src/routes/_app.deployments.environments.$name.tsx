import { createFileRoute } from "@tanstack/react-router";
import EnvironmentPage from "@/views/environments/environment";

export const Route = createFileRoute("/_app/deployments/environments/$name")({ component: EnvironmentPage });
