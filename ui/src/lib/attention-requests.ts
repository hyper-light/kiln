/** Current request identities, plan 01a199e1b177752e84dad5dc990e6a15. Clocks never decide whether a question is new. */
import type { AttentionRequest } from "../api/attention";
import type { AgentInfo } from "../api/harness";

export interface AttentionRequestSet {
  requests: AttentionRequest[];
  unknown: boolean;
}

export function requestsFor(agents: readonly AgentInfo[]): AttentionRequestSet {
  const current: AttentionRequestSet = { requests: [], unknown: false };
  for (const a of agents) {
    const add = (kind: AttentionRequest["kind"], id: string, at: number | undefined) => {
      if (!a.id || !id || at === undefined || !Number.isFinite(at) || at <= 0) current.unknown = true;
      else current.requests.push({ agent: a.id, kind, id, at });
    };
    for (const approval of a.approvals) add("approval", approval.id, approval.at);
    if (a.held) add("held", a.held.why, a.held.since);
    if (a.status === "exited" && a.exit?.error != null && a.exit.again_at == null) add("stop", a.exit.error, a.exit.at);
    if (a.status === "waiting" && a.approvals.length === 0 && !a.held) current.unknown = true;
  }
  return current;
}

export function requestsForFamily(agents: readonly AgentInfo[], id: string): AttentionRequestSet {
  const root = agents.find((a) => a.id === id);
  if (!root) return { requests: [], unknown: true };
  const family = [root];
  const visited = new Set([id]);
  for (const owner of family) {
    for (const child of agents) {
      if (child.subagent !== true || child.parent !== owner.id || visited.has(child.id)) continue;
      visited.add(child.id);
      family.push(child);
    }
  }
  return requestsFor(family);
}

/** A request without a known identity or time wakes conservatively; an exact captured request may remain deferred. */
export function requestsWakePark(current: AttentionRequestSet, deferred: readonly AttentionRequest[] | undefined): boolean {
  return (
    current.unknown ||
    current.requests.some((request) => !deferred?.some((saved) => saved.agent === request.agent && saved.kind === request.kind && saved.id === request.id && saved.at === request.at))
  );
}
