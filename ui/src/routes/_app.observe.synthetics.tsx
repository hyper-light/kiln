import { createFileRoute } from "@tanstack/react-router";
import Synthetics from "@/views/observe/synthetics";

export const Route = createFileRoute("/_app/observe/synthetics")({ component: Synthetics });
