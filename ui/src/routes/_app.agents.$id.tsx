import { createFileRoute } from "@tanstack/react-router";
import AgentPage from "@/views/harness/agent";

export const Route = createFileRoute("/_app/agents/$id")({ component: AgentPage });
