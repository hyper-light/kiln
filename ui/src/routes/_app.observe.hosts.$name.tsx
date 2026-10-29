import { createFileRoute } from "@tanstack/react-router";
import HostPage from "@/views/observe/host";

export const Route = createFileRoute("/_app/observe/hosts/$name")({ component: HostPage });
