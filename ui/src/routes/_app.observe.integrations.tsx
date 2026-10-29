import { createFileRoute } from "@tanstack/react-router";
import Integrations from "@/views/observe/integrations";

export const Route = createFileRoute("/_app/observe/integrations")({ component: Integrations });
