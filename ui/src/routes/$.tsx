/**
 * Every page that hasn't moved into TanStack Router yet: the app as it was (its own router inside),
 * rendered in the browser. Pages leave it for routes of their own one at a time.
 */
import * as React from "react";
import { ClientOnly, createFileRoute } from "@tanstack/react-router";

const App = React.lazy(() => import("@/App").then((m) => ({ default: m.App })));

export const Route = createFileRoute("/$")({
  ssr: false,
  // Never on the server, nor in the shell rendered at build time: what the server sent then always
  // matches what the browser hydrates, on any page.
  component: () => (
    <ClientOnly fallback={null}>
      <App />
    </ClientOnly>
  ),
});
