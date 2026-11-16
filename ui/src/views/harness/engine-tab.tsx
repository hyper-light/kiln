/**
 * An agent's Engine tab, the one tab agent chat gains (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, P2,
 * P3, P5.1, P5.2): the session's sandboxes, its own and its kiln subagents', each whole as its Mission
 * row's dropdown shows it (engine-rows.tsx); then this session's own agent (definition-panel.tsx); then
 * the agents there are, the person's and the system's, found by a filter with facets (as Mission
 * control's sessions are) and sorted, each a row that opens its settings, sessions and experiments in
 * place.
 */
import * as React from "react";
import { agentdefsRes, type AgentDef } from "@/api/agentdefs";
import { FacetBox, type Chosen, type Facet } from "@/components/ui/facet-box";
import { Loadable } from "@/components/ui/loading";
import { FilterBar, SelectField } from "@/components/ui/misc";
import { useRes } from "@/lib/data";
import { useSearchParams } from "@/lib/router";
import { DefinitionPanel } from "./definition-panel";
import { defListKeys, DefRow } from "./definitions";
import { EngineList } from "./engine-rows";
import { useAgentList } from "./use-agents";

export function EngineTab({ agentId }: { agentId: string }) {
  return (
    <div className="in-secs at">
      <Loadable label="Loading its engine" fallback={null}>
        <Engine agentId={agentId} />
      </Loadable>
      <Loadable label="Loading its agent" fallback={null}>
        <DefinitionPanel agentId={agentId} />
      </Loadable>
      <Loadable label="Loading the agents" fallback={null}>
        <AgentList agentId={agentId} />
      </Loadable>
    </div>
  );
}

function Engine({ agentId }: { agentId: string }) {
  const list = useAgentList();
  const a = list.find((x) => x.id === agentId);
  return a ? <EngineList a={a} list={list} /> : null;
}

type Sort = "name" | "name-desc" | "sessions" | "harness";
const SORTS: { value: Sort; label: string }[] = [
  { value: "name", label: "Name A–Z" },
  { value: "name-desc", label: "Name Z–A" },
  { value: "sessions", label: "Most sessions" },
  { value: "harness", label: "Harness" },
];
const ORDER: Record<Sort, (a: AgentDef, b: AgentDef) => number> = {
  name: (a, b) => a.name.localeCompare(b.name),
  "name-desc": (a, b) => b.name.localeCompare(a.name),
  sessions: (a, b) => b.sessions.length - a.sessions.length || a.name.localeCompare(b.name),
  harness: (a, b) => a.harness.localeCompare(b.harness) || a.name.localeCompare(b.name),
};

/** Each facet a definition is found by: whose it is (the person's or the system's), its harness, where it runs. */
const FACETS: { id: string; label: string; of: (d: AgentDef) => string; word: (v: string) => string }[] = [
  { id: "source", label: "Source", of: (d) => (d.own ? "user" : "system"), word: (v) => (v === "user" ? "User" : "System") },
  { id: "harness", label: "Harness", of: (d) => d.harness, word: (v) => v },
  { id: "engine", label: "Runs in", of: (d) => d.runs.engine, word: (v) => (v === "microvm" ? "microVM" : "host") },
];

/** It's shown: its name or harness has the words typed, and each facet chosen has its value (any of a facet's, all facets). */
function shows(d: AgentDef, q: string, chosen: Chosen, except?: string): boolean {
  const words = q.trim().toLowerCase();
  if (words && !`${d.name} ${d.harness}`.toLowerCase().includes(words)) return false;
  return FACETS.every((f) => f.id === except || !chosen[f.id]?.length || chosen[f.id]!.includes(f.of(d)));
}

function AgentList({ agentId }: { agentId: string }) {
  const defs = useRes(agentdefsRes).data;
  const list = useAgentList();
  const [q, setQ] = React.useState("");
  const [chosen, setChosen] = React.useState<Chosen>({ source: ["user"] });
  const [sort, setSort] = React.useState<Sort>("name");
  // The one open lives in the URL (`?open=ID`; kiln's principle 1): its own agent first, as the one
  // this chat is about, until another is opened, or none (`?open=`).
  const [params, setParams] = useSearchParams();
  const open = params.has("open") ? params.get("open") || null : (defs.find((d) => d.own && d.sessions.some((s) => s.id === agentId))?.id ?? null);
  const setOpen = (id: string | null) => {
    const next = new URLSearchParams(params);
    next.set("open", id ?? "");
    setParams(next, { replace: id === null });
  };
  // Each option says how many it would show, the other facets as they are.
  const facets: Facet[] = FACETS.map((f) => ({
    id: f.id,
    label: f.label,
    options: [...new Set(defs.map(f.of))].sort().map((v) => ({ id: v, label: f.word(v), count: defs.filter((d) => f.of(d) === v && shows(d, q, chosen, f.id)).length })),
  }));
  const shown = defs.filter((d) => shows(d, q, chosen)).sort(ORDER[sort]);
  return (
    <section className="at-agents" aria-label="Agents">
      <h3 className="in-sec-head">
        <span className="in-sec-static">
          Agents
          <span className="in-sec-count">
            {shown.length} of {defs.length}
          </span>
        </span>
      </h3>
      <FilterBar className="at-bar">
        <FacetBox label="Filter agents" placeholder="Filter by name, harness, source…" query={q} onQuery={setQ} facets={facets} chosen={chosen} onChosen={setChosen} />
        <SelectField label="Sort agents" hideLabel value={sort} onChange={(v) => setSort(v as Sort)} options={SORTS} />
      </FilterBar>
      {shown.length === 0 ? (
        <p className="in-none">No agent matches.</p>
      ) : (
        <ul className="ms-list" data-list="agents-tab" onKeyDown={(e) => defListKeys(e, open, () => setOpen(null))}>
          {shown.map((d) => (
            <DefRow key={d.id} d={d} list={list} open={open === d.id} onOpen={() => setOpen(open === d.id ? null : d.id)} />
          ))}
        </ul>
      )}
    </section>
  );
}
