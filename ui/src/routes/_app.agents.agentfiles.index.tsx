import { createFileRoute } from "@tanstack/react-router";
import AgentfilesPage from "@/views/agentfiles/agentfiles";

export const Route = createFileRoute("/_app/agents/agentfiles/")({ component: AgentfilesPage });
