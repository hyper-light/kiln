import { createFileRoute } from "@tanstack/react-router";
import Slos from "@/views/observe/slos";

export const Route = createFileRoute("/_app/observe/slos/")({ component: Slos });
