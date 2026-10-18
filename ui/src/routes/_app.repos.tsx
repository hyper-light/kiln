import { createFileRoute } from "@tanstack/react-router";
import Repos from "@/views/repos";

export const Route = createFileRoute("/_app/repos")({ component: Repos });
