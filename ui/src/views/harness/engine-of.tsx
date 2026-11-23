/**
 * Where a session runs: in a microVM, or on its machine itself (bare metal). Only a session in a microVM
 * has sandboxes, settings and experiments to show: its Mission row's groups after its subagents, and the
 * Sandboxes section of agent chat's Engine tab (which every session has).
 */
import { sandboxesRes, type Engine } from "@/api/microvms";
import { useRes } from "@/lib/data";

/**
 * Where a session runs as its start chose (the person's decision, 2026-11-23: Host or microVM is each
 * session's, never its runner's), from its isolation's kind (`AgentInfo.isolation.kind`): in a microVM of
 * its own where it's isolated in one, else on its machine's host.
 */
export const startedIn = (isolation: string | undefined): Engine => (isolation === "microvm" ? "microvm" : "host");

/** Its sandbox's engine, else (none listed yet: just started) what its start chose. */
export function useEngineOf(id: string, isolation: string | undefined): Engine {
  return useRes(sandboxesRes).data[id]?.engine ?? startedIn(isolation);
}
