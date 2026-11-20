import { createFileRoute } from "@tanstack/react-router";
import AgentPage from "@/views/harness/agent";

export const Route = createFileRoute("/_app/agents/$id")({
  beforeLoad: ({ params, location, abortController, preload }) => {
    // W21-R02: a router intent may load code, but only actual navigation reads private data.
    // Imported here, not above: beforeLoad stays in the entry, and the reads' code (the session
    // store, goals, plans) is the conversation's, asked beside its route's chunk (goal 8).
    if (!preload)
      void import("@/lib/agent-navigation").then(({ prepareAgentNavigation }) =>
        prepareAgentNavigation({ id: params.id, tab: new URLSearchParams(location.searchStr).get("tab") === "plan" ? "plan" : "changes", signal: abortController.signal }),
      );
  },
  component: AgentPage,
});
