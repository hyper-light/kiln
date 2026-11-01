import { createFileRoute, redirect } from "@tanstack/react-router";

// Ship's Deploys took the apps' list's place (docs/ci.md W26-R24); an app's page keeps its address.
export const Route = createFileRoute("/_app/deployments/")({
  beforeLoad: () => {
    throw redirect({ to: "/ship/deploys", replace: true });
  },
});
