/**
 * Experiments (docs/ci.md W20-R11): options measured against each other and together, newest
 * first. Each says where it stands: building its variants, how many of its runs are measured,
 * and what it recommends or what was chosen.
 */
import { experimentsRes, type ExperimentRow } from "@/api/experiments";
import { RowsHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { EmptyState, PageIntro } from "@/components/ui/misc";
import { useRes } from "@/lib/data";
import { relativeTime } from "@/lib/format";
import { Link } from "@/lib/router";

export default function ExperimentsPage() {
  return (
    <div className="container-console page">
      <title>Experiments · kiln</title>
      <PageIntro
        index={6}
        label="Agents"
        title="Experiments."
        lead="Options measured against each other and together: each variant is its options' changes on one commit, measured by ordinary runs, with a recommendation."
      />
      <Loadable label="the experiments" fallback={<RowsHold rows={3} rowHeight={64} label="Loading the experiments" />}>
        <List />
      </Loadable>
    </div>
  );
}

/** Where an experiment stands, in a line. */
export function standing(e: Pick<ExperimentRow, "state" | "measured" | "runs" | "recommendation" | "decision">): string {
  switch (e.state) {
    case "implementing":
      return "its options' agents are at work";
    case "building":
      return "building its variants";
    case "measuring":
      return `${e.measured} of ${e.runs} runs measured`;
    case "done":
      return e.decision ? `continuing with ${e.decision.variant}` : e.recommendation ? `recommends ${e.recommendation}` : "no clear winner";
    case "cancelled":
      return "cancelled";
    default:
      return "stopped";
  }
}

function List() {
  const rows = useRes(experimentsRes).data;
  if (!rows.length) {
    return (
      <EmptyState title="No experiments yet">
        An agent starts one when there are options to choose between (<code>experiments.start</code>), and so can you, with <code>kiln experiment start</code>.
      </EmptyState>
    );
  }
  return (
    <ul className="row-list" aria-label="Experiments">
      {rows.map((e) => (
        <li key={e.id} className="flex min-w-0 flex-col gap-1 px-1 py-3">
          <span className="flex min-w-0 flex-wrap items-baseline gap-x-3 gap-y-1">
            <Link to={`/agents/experiments/${encodeURIComponent(e.id)}`} className="text-link font-mono text-[13px]">
              {e.id}
            </Link>
            <span className="min-w-0 text-[13.5px] [overflow-wrap:anywhere] text-[#e4e5e7]">{e.question || `${e.variants} variants`}</span>
          </span>
          <span className="flex min-w-0 flex-wrap gap-x-3 gap-y-0.5 font-mono text-[11px] text-[#8c8f98]">
            <span>{e.repo}</span>
            <span data-state={e.state}>{standing(e)}</span>
            <span>
              by {e.by} · {relativeTime(e.created_at)}
            </span>
          </span>
        </li>
      ))}
    </ul>
  );
}
