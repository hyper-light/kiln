/**
 * An agent's Agents tab (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, P5.1, P5.2): agents managed from
 * chat. This session's own agent first (its definition, version, variant and values, editable in
 * place), then your agents or kiln's, each a row as in Mission's dropdown: where it runs and how fast
 * it starts, Run, and a chevron that opens its definition, sessions and experiment in place.
 */
import * as React from "react";
import { agentdefsRes } from "@/api/agentdefs";
import { Loadable } from "@/components/ui/loading";
import { Segmented } from "@/components/ui/tabs";
import { useRes } from "@/lib/data";
import { DefinitionPanel } from "./definition-panel";
import { DefRow } from "./definitions";
import { Fold } from "./fold";
import { useAgentList } from "./use-agents";

export function AgentsTab({ agentId }: { agentId: string }) {
  return (
    <div className="in-secs at">
      <Loadable label="Loading its agent" fallback={null}>
        <DefinitionPanel agentId={agentId} />
      </Loadable>
      <Loadable label="Loading the agents" fallback={null}>
        <AgentList agentId={agentId} />
      </Loadable>
    </div>
  );
}

function AgentList({ agentId }: { agentId: string }) {
  const defs = useRes(agentdefsRes).data;
  const list = useAgentList();
  const [whose, setWhose] = React.useState<"yours" | "kiln">("yours");
  // Its own agent open first, as the one this chat is about.
  const [open, setOpen] = React.useState<string | null>(() => defs.find((d) => d.own && d.sessions.some((s) => s.id === agentId))?.id ?? null);
  const shown = defs.filter((d) => (whose === "yours" ? d.own : !d.own));
  return (
    <Fold
      id="agents"
      title="Agents"
      count={shown.length}
      aside={
        <Segmented
          size="xs"
          label="Whose"
          value={whose}
          onChange={setWhose}
          options={[
            { value: "yours", label: "Yours", count: defs.filter((d) => d.own).length },
            { value: "kiln", label: "kiln's", count: defs.filter((d) => !d.own).length },
          ]}
        />
      }
    >
      <ul className="ms-list" data-list="agents-tab">
        {shown.map((d) => (
          <DefRow key={d.id} d={d} list={list} open={open === d.id} onOpen={() => setOpen(open === d.id ? null : d.id)} />
        ))}
      </ul>
    </Fold>
  );
}
