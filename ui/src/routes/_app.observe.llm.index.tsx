import { createFileRoute } from "@tanstack/react-router";
import LlmObservability from "@/views/observe/llm";

export const Route = createFileRoute("/_app/observe/llm/")({ component: LlmObservability });
