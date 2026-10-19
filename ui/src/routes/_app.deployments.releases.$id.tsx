import { createFileRoute } from "@tanstack/react-router";
import ReleasePage from "@/views/releases/release";

export const Route = createFileRoute("/_app/deployments/releases/$id")({ component: ReleasePage });
