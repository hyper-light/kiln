import { createFileRoute } from "@tanstack/react-router";
import { harnessSessionsRes, ready, warm } from "@/lib/data";
import Programs from "@/views/programs/index";
import { programsRes } from "@/views/programs/data";

// Its data loads as the route does, beside its code, so the page shows with it (no fallback); the
// agents its workers are chosen from start loading too, and fill the choices once they're in.
export const Route = createFileRoute("/_app/agents/programs/")({
  loader: () => {
    warm(harnessSessionsRes);
    return ready(programsRes);
  },
  component: Programs,
});
