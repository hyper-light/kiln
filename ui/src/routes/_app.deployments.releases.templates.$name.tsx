import { createFileRoute } from "@tanstack/react-router";
import { TemplatePage } from "@/views/releases/templates";

export const Route = createFileRoute("/_app/deployments/releases/templates/$name")({ component: TemplatePage });
