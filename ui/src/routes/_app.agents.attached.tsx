import { createFileRoute } from "@tanstack/react-router";
import AttachedPage from "@/views/attach/index";

export const Route = createFileRoute("/_app/agents/attached")({ component: AttachedPage });
