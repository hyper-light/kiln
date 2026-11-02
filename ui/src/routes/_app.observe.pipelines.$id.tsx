import { createFileRoute } from "@tanstack/react-router";
import PipelinePage from "@/views/observe/pipeline";

export const Route = createFileRoute("/_app/observe/pipelines/$id")({ component: PipelinePage });
