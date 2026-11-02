import { createFileRoute } from "@tanstack/react-router";
import Scanner from "@/views/observe/scanner";

export const Route = createFileRoute("/_app/observe/scanner/")({ component: Scanner });
