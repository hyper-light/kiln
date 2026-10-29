import { createFileRoute } from "@tanstack/react-router";
import { IssuePage } from "@/views/observe/errors";

export const Route = createFileRoute("/_app/observe/errors/$id")({ component: IssuePage });
