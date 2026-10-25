import { createFileRoute } from "@tanstack/react-router";
import EnvironmentsBoard from "@/views/environments/board";

export const Route = createFileRoute("/_app/deployments/environments/")({ component: EnvironmentsBoard });
