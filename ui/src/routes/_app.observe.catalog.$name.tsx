import { createFileRoute } from "@tanstack/react-router";
import CatalogServicePage from "@/views/observe/catalog-service";

export const Route = createFileRoute("/_app/observe/catalog/$name")({ component: CatalogServicePage });
