import { createFileRoute } from "@tanstack/react-router";
import { HostPullsPage } from "@/views/review/host";

export const Route = createFileRoute("/_app/review/hosts/$repo/")({
  component: HostPullsPage,
});
