/** The live host metrics live in Observe now. */
import { createFileRoute, redirect } from "@tanstack/react-router";

export const Route = createFileRoute("/_app/telemetry")({
  beforeLoad: () => {
    throw redirect({ to: "/observe/live", replace: true });
  },
});
