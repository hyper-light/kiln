import { createFileRoute } from "@tanstack/react-router";
import Pipelines from "@/views/observe/pipelines";

export const Route = createFileRoute("/_app/observe/pipelines/")({ component: Pipelines });
