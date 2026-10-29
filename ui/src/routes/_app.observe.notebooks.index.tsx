import { createFileRoute } from "@tanstack/react-router";
import Notebooks from "@/views/observe/notebooks";

export const Route = createFileRoute("/_app/observe/notebooks/")({ component: Notebooks });
