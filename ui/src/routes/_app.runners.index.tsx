import { createFileRoute } from "@tanstack/react-router";
import Runners from "@/views/runners";

export const Route = createFileRoute("/_app/runners/")({ component: Runners });
