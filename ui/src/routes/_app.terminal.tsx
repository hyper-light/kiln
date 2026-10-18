/** The terminal: kiln's shell keeps it mounted (and shows it here), so the page itself is empty. */
import { createFileRoute } from "@tanstack/react-router";

export const Route = createFileRoute("/_app/terminal")({ component: () => null });
