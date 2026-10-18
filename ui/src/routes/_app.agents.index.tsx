import { createFileRoute } from "@tanstack/react-router";
import Mission from "@/views/harness/mission";

export const Route = createFileRoute("/_app/agents/")({ component: Mission });
