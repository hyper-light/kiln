/** Plans are kept per agent session now (each agent's Plan tab): the old list's address goes to the agents. */
import { createFileRoute, redirect } from "@tanstack/react-router";

export const Route = createFileRoute("/_app/agents/plans/")({
  beforeLoad: () => {
    throw redirect({ to: "/agents", replace: true });
  },
});
