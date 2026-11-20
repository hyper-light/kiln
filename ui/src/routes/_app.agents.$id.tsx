import { createFileRoute } from "@tanstack/react-router";
import { isAuthed } from "@/api/client";
import { startSession } from "@/api/session-socket";
import AgentPage from "@/views/harness/agent";

export const Route = createFileRoute("/_app/agents/$id")({
  beforeLoad: ({ params, location, abortController, preload }) => {
    // W21-R02: a router intent may load code, but only actual navigation reads private data.
    if (preload || typeof window === "undefined" || isAuthed() === false) return;
    // The conversation's session starts here, before the page's code is in (session-socket.ts:
    // its socket, which its store takes); the rest of what the page reads first comes with that
    // code (lib/agent-navigation.ts, imported as it runs: beforeLoad stays in the shell, goal 8).
    startSession(params.id, abortController.signal);
    void import("@/lib/agent-navigation").then(({ prepareAgentNavigation }) =>
      prepareAgentNavigation({ id: params.id, tab: new URLSearchParams(location.searchStr).get("tab") === "plan" ? "plan" : "changes", signal: abortController.signal }),
    );
  },
  component: AgentPage,
});
