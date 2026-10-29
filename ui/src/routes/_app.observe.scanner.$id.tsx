import { createFileRoute } from "@tanstack/react-router";
import ScanningGroupPage from "@/views/observe/scanner-group";

export const Route = createFileRoute("/_app/observe/scanner/$id")({ component: ScanningGroupPage });
