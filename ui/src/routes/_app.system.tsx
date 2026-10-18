import { createFileRoute } from "@tanstack/react-router";
import System from "@/views/system";

export const Route = createFileRoute("/_app/system")({ component: System });
