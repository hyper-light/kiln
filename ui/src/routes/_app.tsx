/**
 * Every signed-in page: the sign-in gate and kiln's shell, with the page in it. In the browser for
 * now (ClientOnly: what the server sends always matches what the browser hydrates); pages render on
 * the server, as server components, one at a time.
 */
import { ClientOnly, createFileRoute } from "@tanstack/react-router";
import { SignedIn, Splash } from "@/App";

export const Route = createFileRoute("/_app")({
  ssr: false,
  component: () => (
    <ClientOnly fallback={<Splash />}>
      <SignedIn />
    </ClientOnly>
  ),
});
