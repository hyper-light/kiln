import { createFileRoute } from "@tanstack/react-router";
import ReleaseApp from "@/views/releases/app";

export const Route = createFileRoute("/_app/deployments/releases/apps/$app")({ component: ReleaseApp });
