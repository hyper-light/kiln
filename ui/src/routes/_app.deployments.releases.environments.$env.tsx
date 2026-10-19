import { createFileRoute } from "@tanstack/react-router";
import ReleaseEnvironment from "@/views/releases/environment";

export const Route = createFileRoute("/_app/deployments/releases/environments/$env")({ component: ReleaseEnvironment });
