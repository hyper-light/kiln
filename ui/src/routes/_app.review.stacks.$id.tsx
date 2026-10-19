import { createFileRoute } from "@tanstack/react-router";
import StackPage from "@/views/review/stack";

export const Route = createFileRoute("/_app/review/stacks/$id")({ component: StackPage });
