import { createFileRoute } from "@tanstack/react-router";
import ResearchPage from "@/views/research/research";

export const Route = createFileRoute("/_app/agents/research/$id")({ component: ResearchPage });
