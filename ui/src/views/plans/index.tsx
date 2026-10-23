/**
 * Plans: what an agent will do, kept apart from any one session. Written here or saved from an
 * agent's plan, changed (each change a version), shared by a link, and run on any agent.
 */
import * as React from "react";
import { Link } from "@/lib/router";
import { Plus } from "lucide-react";
import { planHref, type Plan } from "@/api/plans";
import { Button } from "@/components/ui/button";
import { FacetBox, type Chosen, type Facet } from "@/components/ui/facet-box";
import { RowsHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { EmptyState, PageIntro, SelectField } from "@/components/ui/misc";
import { useRes } from "@/lib/data";
import { plansRes } from "./data";
import { relativeTime } from "@/lib/format";
import { ResearchList } from "@/views/research/research";
import "./plans.css";

const SORTS = [
  { value: "changed", label: "Changed, newest" },
  { value: "title", label: "Title, A to Z" },
  { value: "runs", label: "Run most" },
];

const FACETS = [
  { id: "tag", label: "Tags" },
  { id: "from", label: "From" },
  { id: "shared", label: "Sharing" },
];

function values(p: Plan): Record<string, string[]> {
  return {
    tag: p.tags,
    from: [p.source?.harness ? `an agent (${p.source.harness})` : "written here"],
    shared: [p.share ? "shared" : "not shared"],
  };
}

export default function Plans() {
  return (
    <div className="container-console page plans-page">
      <title>Plans · kiln</title>
      <PageIntro
        index={6}
        label="Agents / Plans"
        title="Plans."
        lead="What an agent will do, kept: write one, or save one an agent made. Change it, share it, and run it on any agent, as one message or step by step."
        actions={
          <Button asChild size="sm">
            <Link to="/agents/plans/new">
              <Plus /> New plan
            </Link>
          </Button>
        }
      />
      <Loadable label="the plans" fallback={<PlansHold />}>
        <PlansBody />
      </Loadable>
      {/* What agents found out, each graded by kiln: a plan's planning, or any question asked of them. */}
      <Loadable label="the research" fallback={null}>
        <ResearchList />
      </Loadable>
    </div>
  );
}

function PlansTools({
  q,
  onQ,
  facets,
  chosen,
  onChosen,
  sort,
  onSort,
}: {
  q: string;
  onQ: (v: string) => void;
  facets: Facet[];
  chosen: Chosen;
  onChosen: (c: Chosen) => void;
  sort: string;
  onSort: (v: string) => void;
}) {
  return (
    <div className="plans-tools">
      <FacetBox className="plans-filter" label="Filter plans" placeholder="Filter by title, text, tag…" query={q} onQuery={onQ} facets={facets} chosen={chosen} onChosen={onChosen} />
      <SelectField className="plans-sort" label="Sort plans" hideLabel value={sort} onChange={onSort} options={SORTS} />
    </div>
  );
}

function PlansHold() {
  const noop = () => undefined;
  return (
    <>
      <PlansTools q="" onQ={noop} facets={[]} chosen={{}} onChosen={noop} sort="changed" onSort={noop} />
      <RowsHold rows={4} rowHeight={78} label="Loading the plans" />
    </>
  );
}

function PlansBody() {
  const all = useRes(plansRes).data;
  const [q, setQ] = React.useState("");
  const [chosen, setChosen] = React.useState<Chosen>({});
  const [sort, setSort] = React.useState("changed");

  const facets: Facet[] = React.useMemo(
    () =>
      FACETS.map((f) => {
        const n = new Map<string, number>();
        for (const p of all) for (const v of values(p)[f.id] ?? []) n.set(v, (n.get(v) ?? 0) + 1);
        return { id: f.id, label: f.label, options: [...n.entries()].sort((a, b) => a[0].localeCompare(b[0])).map(([id, count]) => ({ id, label: id, count })) };
      }).filter((f) => f.options.length > 0),
    [all],
  );

  const ql = q.trim().toLowerCase();
  const shown = all
    .filter((p) => Object.entries(chosen).every(([f, want]) => want.length === 0 || (values(p)[f] ?? []).some((v) => want.includes(v))))
    .filter((p) => !ql || `${p.title} ${p.body} ${p.tags.join(" ")}`.toLowerCase().includes(ql))
    .sort((a, b) => (sort === "title" ? a.title.localeCompare(b.title) : sort === "runs" ? b.runs - a.runs || b.updated_at - a.updated_at : b.updated_at - a.updated_at));

  if (all.length === 0) {
    return (
      <EmptyState
        title="No plans yet"
        action={
          <Button asChild size="sm" variant="secondary">
            <Link to="/agents/plans/new">New plan</Link>
          </Button>
        }
      >
        Write one here, or press Save plan on a plan an agent proposes. Agents can keep their plans here too (kiln&rsquo;s MCP tools, plans_create).
      </EmptyState>
    );
  }
  return (
    <>
      <PlansTools q={q} onQ={setQ} facets={facets} chosen={chosen} onChosen={setChosen} sort={sort} onSort={setSort} />
      {shown.length === 0 ? (
        <EmptyState title="No plans match." />
      ) : (
        <ul className="plans-list">
          {shown.map((p) => (
            <li key={p.id} className="plans-row">
              <Link to={planHref(p.id)} className="plans-row-title">
                {p.title}
              </Link>
              <div className="plans-row-meta">
                {p.steps.length ? `${p.steps.length} step${p.steps.length === 1 ? "" : "s"}` : "one message"} · v{p.version} · changed {relativeTime(p.updated_at)}
                {p.runs ? ` · run ${p.runs}×` : ""}
                {p.share ? " · shared" : ""}
                {p.research ? " · researched" : ""}
                {p.source?.agent ? ` · from ${p.source.agent}` : ""}
              </div>
              {p.tags.length > 0 && (
                <div className="plans-tags">
                  {p.tags.map((t) => (
                    <span key={t} className="plans-tag">
                      {t}
                    </span>
                  ))}
                </div>
              )}
            </li>
          ))}
        </ul>
      )}
    </>
  );
}
