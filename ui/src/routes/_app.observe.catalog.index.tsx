import { createFileRoute } from "@tanstack/react-router";
import Catalog from "@/views/observe/catalog";

export const Route = createFileRoute("/_app/observe/catalog/")({ component: Catalog });
