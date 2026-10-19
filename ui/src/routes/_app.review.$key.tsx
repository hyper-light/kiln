import { createFileRoute } from "@tanstack/react-router";
import ReviewPage from "@/views/review/page";

export const Route = createFileRoute("/_app/review/$key")({ component: ReviewPage });
