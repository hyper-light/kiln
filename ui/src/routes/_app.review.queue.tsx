import { createFileRoute } from "@tanstack/react-router";
import QueuePage from "@/views/review/queue";

export const Route = createFileRoute("/_app/review/queue")({ component: QueuePage });
