import { createFileRoute } from "@tanstack/react-router";
import ErrorTracking from "@/views/observe/errors";

export const Route = createFileRoute("/_app/observe/errors/")({ component: ErrorTracking });
