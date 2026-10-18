import { createFileRoute } from "@tanstack/react-router";
import Workflows from "@/views/ci/workflows";

export const Route = createFileRoute("/_app/workflows")({ component: Workflows });
