import { createFileRoute } from "@tanstack/react-router";
import ObserveHome from "@/views/observe/home";

export const Route = createFileRoute("/_app/observe/")({ component: ObserveHome });
