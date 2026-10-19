import { createFileRoute } from "@tanstack/react-router";
import ReleaseRegistration from "@/views/releases/registration";

export const Route = createFileRoute("/_app/deployments/releases/apps/$app_/environments/$env")({ component: ReleaseRegistration });
