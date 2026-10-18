import { createFileRoute } from "@tanstack/react-router";
import Runs from "@/views/runs";

export const Route = createFileRoute("/_app/runs/")({ component: Runs });
