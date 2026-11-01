import { createFileRoute, redirect } from "@tanstack/react-router";

// Ship's tabs are Commits and Deploys (docs/ci.md W26-R24): the runs' list lands on Ship; a run's page keeps its address.
export const Route = createFileRoute("/_app/runs/")({
  beforeLoad: () => {
    throw redirect({ to: "/ship", replace: true });
  },
});
