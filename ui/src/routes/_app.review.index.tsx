import { createFileRoute } from "@tanstack/react-router";
import InboxPage from "@/views/review/inbox";

export const Route = createFileRoute("/_app/review/")({ component: InboxPage });
