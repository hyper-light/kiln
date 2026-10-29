import { createFileRoute } from "@tanstack/react-router";
import { TracePage } from "@/views/observe/traces";

export const Route = createFileRoute("/_app/observe/traces/$id")({ component: TracePage });
