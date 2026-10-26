import { createFileRoute } from "@tanstack/react-router";
import HostPullPage from "@/views/review/host";

export const Route = createFileRoute("/_app/review/hosts/$repo/$number")({
  component: HostPullPage,
});
