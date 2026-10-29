import { createFileRoute } from "@tanstack/react-router";
import Profiles from "@/views/observe/profiles";

export const Route = createFileRoute("/_app/observe/profiles")({ component: Profiles });
