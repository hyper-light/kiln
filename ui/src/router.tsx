/**
 * kiln's router (TanStack Router, under TanStack Start). Pages move into it from the app it still
 * hosts (`routes/$.tsx`) one at a time.
 */
import { createRouter } from "@tanstack/react-router";
import { routeTree } from "./routeTree.gen";

export function getRouter() {
  return createRouter({ routeTree, scrollRestoration: true });
}

declare module "@tanstack/react-router" {
  interface Register {
    router: ReturnType<typeof getRouter>;
  }
}
