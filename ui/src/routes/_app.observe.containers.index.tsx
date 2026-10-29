import { createFileRoute } from "@tanstack/react-router";
import Containers from "@/views/observe/containers";

export const Route = createFileRoute("/_app/observe/containers/")({ component: Containers });
