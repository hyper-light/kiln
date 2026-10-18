import { createFileRoute } from "@tanstack/react-router";
import Overview from "@/views/overview";

export const Route = createFileRoute("/_app/")({ component: Overview });
