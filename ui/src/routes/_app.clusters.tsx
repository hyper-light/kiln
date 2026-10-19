import { createFileRoute } from "@tanstack/react-router";
import Clusters from "@/views/clusters";

export const Route = createFileRoute("/_app/clusters")({ component: Clusters });
