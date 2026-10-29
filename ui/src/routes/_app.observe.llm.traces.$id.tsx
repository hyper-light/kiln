import { createFileRoute } from "@tanstack/react-router";
import { LlmTracePage } from "@/views/observe/llm";

export const Route = createFileRoute("/_app/observe/llm/traces/$id")({ component: LlmTracePage });
