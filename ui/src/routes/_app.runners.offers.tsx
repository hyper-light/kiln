import { createFileRoute } from "@tanstack/react-router";
import Offers from "@/views/offers";

export const Route = createFileRoute("/_app/runners/offers")({ component: Offers });
