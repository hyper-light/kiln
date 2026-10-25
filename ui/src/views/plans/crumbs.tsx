/**
 * The way back from a plan, a plan's run or research: Agents, then the agent session it's kept
 * with, opened on its plans (its Plan tab). Plans are kept per agent session; none here, Agents.
 */
import * as React from "react";
import { Crumbs } from "@/components/ui/misc";
import { harnessSessionsRes, useRes } from "@/lib/data";

function SessionName({ id }: { id: string }) {
  const name = useRes(harnessSessionsRes).data.find((a) => a.id === id)?.name;
  return <>{name || `Agent ${id.slice(0, 8)}`}</>;
}

export function SessionCrumbs({ session, more = [] }: { session?: string | null; more?: { label: React.ReactNode; to?: string }[] }) {
  const back = session
    ? [
        {
          label: (
            <React.Suspense fallback={`Agent ${session.slice(0, 8)}`}>
              <SessionName id={session} />
            </React.Suspense>
          ),
          to: `/agents/${encodeURIComponent(session)}?tab=plan`,
        },
      ]
    : [];
  return <Crumbs index={6} items={[{ label: "Agents", to: "/agents" }, ...back, ...more]} />;
}
