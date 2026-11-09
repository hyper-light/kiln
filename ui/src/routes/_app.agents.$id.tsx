import { createFileRoute } from "@tanstack/react-router";
import { prepareAgentNavigation } from "@/lib/agent-navigation";
import AgentPage from "@/views/harness/agent";

export const Route = createFileRoute("/_app/agents/$id")({
  beforeLoad: ({ params, location, abortController, preload }) => {
    // W21-R02: a router intent may load code, but only actual navigation reads private data.
    if (!preload) void prepareAgentNavigation({ id: params.id, tab: new URLSearchParams(location.searchStr).get("tab") === "plan" ? "plan" : "changes", signal: abortController.signal });
  },
  component: AgentPage,
});
