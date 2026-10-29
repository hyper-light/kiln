import { createFileRoute } from "@tanstack/react-router";
import ContainerPage from "@/views/observe/container";

export const Route = createFileRoute("/_app/observe/containers/$id")({ component: ContainerPage });
