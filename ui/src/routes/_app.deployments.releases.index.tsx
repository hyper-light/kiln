import { createFileRoute } from "@tanstack/react-router";
import Releases from "@/views/releases/index";

export const Route = createFileRoute("/_app/deployments/releases/")({ component: Releases });
