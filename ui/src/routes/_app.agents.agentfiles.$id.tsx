import { createFileRoute } from "@tanstack/react-router";
import AgentfilePage from "@/views/agentfiles/agentfile";

export const Route = createFileRoute("/_app/agents/agentfiles/$id")({ component: AgentfilePage });
