/**
 * Where a session runs: in a microVM, or on its machine itself (bare metal). Only a session in a microVM
 * has sandboxes, settings and experiments to show: its Mission row's groups after its subagents, and agent
 * chat's Engine tab.
 */
import * as React from "react";
import { engineOf, sandboxesRes, type Engine } from "@/api/microvms";
import { agentsRes, useRes } from "@/lib/data";

/** Its sandbox's engine, else (none listed yet: just started) its runner's. */
export function useEngineOf(id: string, target: string): Engine | null {
  const brief = useRes(sandboxesRes).data[id];
  const machine = target.replace(/^agent:/, "");
  const facts = useRes(agentsRes).data.find((r) => r.name === machine)?.facts;
  return brief?.engine ?? engineOf(facts)?.engine ?? null;
}

/** What only a session in a microVM shows. */
export function OnMicroVm({ id, target, children }: { id: string; target: string; children: React.ReactNode }) {
  return useEngineOf(id, target) === "microvm" ? children : null;
}
