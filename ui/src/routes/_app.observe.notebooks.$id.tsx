import { createFileRoute } from "@tanstack/react-router";
import NotebookPage from "@/views/observe/notebook";

export const Route = createFileRoute("/_app/observe/notebooks/$id")({ component: NotebookPage });
