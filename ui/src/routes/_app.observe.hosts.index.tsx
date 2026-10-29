import { createFileRoute } from "@tanstack/react-router";
import Hosts from "@/views/observe/hosts";

export const Route = createFileRoute("/_app/observe/hosts/")({ component: Hosts });
