import { createFileRoute } from "@tanstack/react-router";
import NotFound from "@/views/not-found";

export const Route = createFileRoute("/_app/$")({ component: NotFound });
