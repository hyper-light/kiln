import { createFileRoute } from "@tanstack/react-router";
import CommitPage from "@/views/ship/commit";

export const Route = createFileRoute("/_app/ship_/$owner/$name/$sha")({ component: CommitPage });
