/** A shared plan: for anyone with its link, signed in or not. */
import { ClientOnly, createFileRoute } from "@tanstack/react-router";
import { Providers, Splash } from "@/App";
import SharedPlan from "@/views/plans/shared";

export const Route = createFileRoute("/shared/plans/$token")({
  ssr: false,
  component: () => (
    <ClientOnly fallback={<Splash />}>
      <Providers>
        <SharedPlan />
      </Providers>
    </ClientOnly>
  ),
});
