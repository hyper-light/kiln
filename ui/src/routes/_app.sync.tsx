/** Sync lives in Repositories now. */
import { createFileRoute, redirect } from "@tanstack/react-router";

export const Route = createFileRoute("/_app/sync")({
  beforeLoad: () => {
    throw redirect({ to: "/repos", search: { show: "synced" } as never, replace: true });
  },
});
