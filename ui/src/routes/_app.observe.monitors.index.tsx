import { createFileRoute } from "@tanstack/react-router";
import Monitors from "@/views/observe/monitors";

export const Route = createFileRoute("/_app/observe/monitors/")({ component: Monitors });
