import { createFileRoute } from "@tanstack/react-router";
import FoundRedirect from "@/views/harness/found";

export const Route = createFileRoute("/_app/agents/sessions")({ component: FoundRedirect });
