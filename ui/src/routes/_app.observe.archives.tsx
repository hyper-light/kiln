import { createFileRoute } from "@tanstack/react-router";
import Archives from "@/views/observe/archives";

export const Route = createFileRoute("/_app/observe/archives")({ component: Archives });
