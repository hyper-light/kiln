import { createFileRoute } from "@tanstack/react-router";
import Indexes from "@/views/observe/indexes";

export const Route = createFileRoute("/_app/observe/indexes")({ component: Indexes });
