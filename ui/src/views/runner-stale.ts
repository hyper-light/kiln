import type { Agent } from "@/api/types";

/** Why a runner's record is stale (docs/machines.md, "Records that are stale"): it never said which machine it is, so kiln can't fold it into one, and only a person forgets it. Null for one that isn't. */
export function staleWhy(agent: Pick<Agent, "stale" | "connected">): string | null {
  if (!agent.stale || agent.connected) return null;
  return "Stale: its runner never said its machine id, so kiln can't tell which machine it is or fold it into one. Forget it if no machine runs it any more; a runner of it that connects again comes back as a record like any other.";
}

/** What a runner's state says in the list: offline, or stale for a record no machine is proven to be. */
export function offlineLabel(agent: Pick<Agent, "stale">): "stale" | "offline" {
  return agent.stale ? "stale" : "offline";
}
