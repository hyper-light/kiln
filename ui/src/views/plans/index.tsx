/**
 * Plans: what an agent will do, kept apart from any one session. Written here or saved from an
 * agent's plan, changed (each change a version), shared by a link, and run on any agent.
 */
import * as React from "react";
import { Link } from "react-router";
import { Plus } from "lucide-react";
import { planHref, plansApi, type Plan } from "@/api/plans";
import { Button } from "@/components/ui/button";
import { FacetBox, type Chosen, type Facet } from "@/components/ui/facet-box";
import { Loadable } from "@/components/ui/loading";
import { EmptyState, PageIntro, SelectField } from "@/components/ui/misc";
import { relativeTime } from "@/lib/format";
import { useDocumentTitle } from "@/lib/hooks";
import { useSuspenseResource } from "@/lib/resource";
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
  useDocumentTitle("Plans");
  return (
    <div className="container-console page plans-page">
      <PageIntro
        index={6}
        label="Agents / Plans"
        title="Plans."
        lead="What an agent will do, kept: write one, or save one an agent made. Change it, share it, and run it on any agent, as one message, step by step, or as goals."
        actions={
          <Button asChild size="sm">
            <Link to="/agents/plans/new">
              <Plus /> New plan
            </Link>
          </Button>
        }
      />
      <Loadable label="Loading plans">
        <PlanList />
      </Loadable>
    </div>
  );
}

function PlanList() {
  const res = useSuspenseResource("plans", (s) => plansApi.list(s), { pollMs: 20_000 });
  const [q, setQ] = React.useState("");
  const [chosen, setChosen] = React.useState<Chosen>({});
  const [sort, setSort] = React.useState("changed");
  const all = res.data;

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

  return all.length === 0 ? (
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
  ) : (
    <>
      <div className="plans-tools">
        <FacetBox className="plans-filter" label="Filter plans" placeholder="Filter by title, text, tag…" query={q} onQuery={setQ} facets={facets} chosen={chosen} onChosen={setChosen} />
        <SelectField className="plans-sort" label="Sort plans" hideLabel value={sort} onChange={setSort} options={SORTS} />
      </div>
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
