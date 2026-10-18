import { createFileRoute } from "@tanstack/react-router";
import Plans from "@/views/plans/index";

export const Route = createFileRoute("/_app/agents/plans/")({ component: Plans });
